// Copyright 2017 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package build

import (
	"bufio"
	"encoding/binary"
	"encoding/json"
	"fmt"
	"io"
	"io/ioutil"
	"os"
	"path/filepath"
	"regexp"
	"runtime"
	"sync"

	"go.fuchsia.dev/fuchsia/src/sys/pkg/bin/pm/pkg"
	"go.fuchsia.dev/fuchsia/src/sys/pkg/lib/far/go"
	"go.fuchsia.dev/fuchsia/src/sys/pkg/lib/merkle"
)

const abiRevisionKey string = "meta/fuchsia.abi/abi-revision"

// invalidRepositoryCharsPattern contains all characters not allowed by the spec in
// https://fuchsia.dev/fuchsia-src/concepts/packages/package_url#repository
var InvalidRepositoryCharsPattern = regexp.MustCompile("[^a-z0-9-.]").MatchString

// PackageManifest is the json structure representation of a full package
// manifest.
type PackageManifest struct {
	Version    string            `json:"version"`
	Repository string            `json:"repository,omitempty"`
	Package    pkg.Package       `json:"package"`
	Blobs      []PackageBlobInfo `json:"blobs"`
}

// packageManifestMaybeRelative is the json structure representation of a package
// manifest that may contain file-relative source paths. This is a separate type
// from PackageManifest so we don't need to touch every use of PackageManifest to
// avoid writing invalid blob_sources_relative values to disk.
type packageManifestMaybeRelative struct {
	Version    string            `json:"version"`
	Repository string            `json:"repository,omitempty"`
	Package    pkg.Package       `json:"package"`
	Blobs      []PackageBlobInfo `json:"blobs"`
	RelativeTo string            `json:"blob_sources_relative"`
}

// LoadPackageManifest parses the package manifest for a particular package,
// resolving file-relative blob source paths before returning if needed.
func LoadPackageManifest(packageManifestPath string) (*PackageManifest, error) {
	fileContents, err := ioutil.ReadFile(packageManifestPath)
	if err != nil {
		return nil, fmt.Errorf("failed to read %s: %w", packageManifestPath, err)
	}

	rawManifest := &packageManifestMaybeRelative{}
	if err := json.Unmarshal(fileContents, rawManifest); err != nil {
		return nil, fmt.Errorf("failed to unmarshal %s: %w", packageManifestPath, err)
	}

	manifest := &PackageManifest{}
	manifest.Version = rawManifest.Version
	manifest.Repository = rawManifest.Repository
	manifest.Package = rawManifest.Package

	if manifest.Version != "1" {
		return nil, fmt.Errorf("unknown version %q, can't load manifest", manifest.Version)
	}

	// if the manifest has file-relative blob paths, make them relative to the working directory
	if rawManifest.RelativeTo == "file" {
		basePath := filepath.Dir(packageManifestPath)
		for i := 0; i < len(rawManifest.Blobs); i++ {
			blob := rawManifest.Blobs[i]
			blob.SourcePath = filepath.Join(basePath, blob.SourcePath)
			manifest.Blobs = append(manifest.Blobs, blob)
		}
	} else {
		manifest.Blobs = rawManifest.Blobs
	}

	return manifest, nil
}

// Init initializes package metadata in the output directory. A manifest
// is generated with a name matching the output directory name.
func Init(cfg *Config) error {
	metadir := filepath.Join(cfg.OutputDir, "meta")
	if err := os.MkdirAll(metadir, os.ModePerm); err != nil {
		return err
	}

	meta := filepath.Join(metadir, "package")
	if _, err := os.Stat(meta); os.IsNotExist(err) {
		f, err := os.Create(meta)
		if err != nil {
			return err
		}

		p, err := cfg.Package()
		if err != nil {
			return err
		}

		err = json.NewEncoder(f).Encode(&p)
		f.Close()
		if err != nil {
			return err
		}
	}
	return nil
}

// Update walks the contents of the package and updates the merkle root values
// within the contents file.
func Update(cfg *Config) error {
	metadir := filepath.Join(cfg.OutputDir, "meta")
	os.MkdirAll(metadir, os.ModePerm)
	manifest, err := cfg.Manifest()
	if err != nil {
		return err
	}

	if err := writeABIRevision(cfg, manifest); err != nil {
		return err
	}

	contentsPath := filepath.Join(metadir, "contents")
	pkgContents := manifest.Content()

	// manifestLines is a channel containing unpacked manifest paths
	var manifestLines = make(chan struct{ src, dest string }, len(pkgContents))
	go func() {
		for dest, src := range pkgContents {
			manifestLines <- struct{ src, dest string }{src, dest}
		}
		close(manifestLines)
	}()

	// contentCollector receives entries to include in contents
	type contentEntry struct {
		path string
		root MerkleRoot
	}
	var contentCollector = make(chan contentEntry, len(pkgContents))
	var errors = make(chan error)

	// w is a group that is done when contentCollector is fully populated
	var w sync.WaitGroup
	for i := runtime.NumCPU(); i > 0; i-- {
		w.Add(1)

		go func() {
			defer w.Done()

			for in := range manifestLines {
				var t merkle.Tree
				cf, err := os.Open(in.src)
				if err != nil {
					errors <- fmt.Errorf("build.Update: open %s for %s: %s", in.src, in.dest, err)
					return
				}
				_, err = t.ReadFrom(bufio.NewReader(cf))
				cf.Close()
				if err != nil {
					errors <- err
					return
				}

				var root MerkleRoot
				copy(root[:], t.Root())
				contentCollector <- contentEntry{in.dest, root}
			}
		}()
	}

	// close the collector channel when all workers are done
	go func() {
		w.Wait()
		close(contentCollector)
	}()

	// collect all results and close done to signal the waiting select
	var done = make(chan struct{})
	contents := MetaContents{}
	go func() {
		for entry := range contentCollector {
			contents[entry.path] = entry.root
		}
		close(done)
	}()

	select {
	case <-done:
		// contents is populated
	case err := <-errors:
		// exit on the first error
		return err
	}

	manifest.Paths["meta/contents"] = contentsPath

	return ioutil.WriteFile(contentsPath,
		[]byte(contents.String()), os.ModePerm)
}

func writeABIRevision(cfg *Config, manifest *Manifest) error {
	// FIXME(): We can stop treating the ABI revision as optional once the
	// ecosystem has migrated to specifying it everywhere.
	if cfg.PkgABIRevision == 0 {
		return nil
	}

	abiDir := filepath.Join(cfg.OutputDir, "meta", "fuchsia.abi")
	os.MkdirAll(abiDir, os.ModePerm)

	b := make([]byte, 8)
	binary.LittleEndian.PutUint64(b, cfg.PkgABIRevision)

	path := filepath.Join(abiDir, "abi-revision")
	if err := ioutil.WriteFile(path, b, os.ModePerm); err != nil {
		return err
	}

	manifest.Paths[abiRevisionKey] = path

	return nil
}

// ErrRequiredFileMissing is returned by operations when the operation depends
// on a file that was not found on disk.
type ErrRequiredFileMissing struct {
	Path string
}

func (e ErrRequiredFileMissing) Error() string {
	return fmt.Sprintf("pkg: missing required file: %q", e.Path)
}

// RequiredFiles is a list of files that are required before a package can be sealed.
var RequiredFiles = []string{"meta/contents", "meta/package"}

// Validate ensures that the package contains the required files.
func Validate(cfg *Config) error {
	if InvalidRepositoryCharsPattern(cfg.PkgRepository) {
		return fmt.Errorf("pkg: invalid package repository \"%v\"", cfg.PkgRepository)
	}
	manifest, err := cfg.Manifest()
	if err != nil {
		return err
	}
	meta := manifest.Meta()

	for _, f := range RequiredFiles {
		if _, ok := meta[f]; !ok {
			return ErrRequiredFileMissing{f}
		}
	}

	return nil
}

// Seal archives meta/ into a FAR archive named meta.far.
func Seal(cfg *Config) (string, error) {
	manifest, err := cfg.Manifest()
	if err != nil {
		return "", err
	}

	if err := Validate(cfg); err != nil {
		return "", err
	}

	archive, err := os.Create(cfg.MetaFAR())
	if err != nil {
		return "", err
	}

	if err := far.Write(archive, manifest.Meta()); err != nil {
		return "", err
	}

	if _, err := archive.Seek(0, io.SeekStart); err != nil {
		return "", err
	}

	var tree merkle.Tree
	if _, err := tree.ReadFrom(archive); err != nil {
		return "", err
	}
	if err := ioutil.WriteFile(cfg.MetaFARMerkle(), []byte(fmt.Sprintf("%x", tree.Root())), os.ModePerm); err != nil {
		return "", err
	}
	return cfg.MetaFAR(), archive.Close()
}
