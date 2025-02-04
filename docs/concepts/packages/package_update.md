# Developing with Fuchsia packages

Almost everything that exists on a Fuchsia system is a [Fuchsia package][pkg-struct].
Whether it is immediately apparent or not almost everything you see on
Fuchsia lives in a package. This document will cover the basics of a
package-driven workflow where you [build][pkg-doc] a package and push it to a
Fuchsia device that is reachable via IP from your development host.

## Pre-requisites and overview

The host and target must be able to communicate over IP. In particular
it must be possible to SSH from the development host to the target device, and
the target device must be able to connect via TCP to the development host on
port 8083. The SSH connection is used to issue commands to the target device.

The development host will run a simple, static file, HTTP server that makes the
updates available to the target. This HTTP server is part of the Fuchsia source
code and built automatically.

The target is instructed to look for changes on the development host via a
couple of commands that are run manually. When the update system on the target
sees these changes it will fetch the new software from the HTTP server running
on the host. The new software will be available until the target is rebooted.

## Building

<!-- TODO(jmatt): improve to talk about wider variety of build options -->

To build a package containing the required code, a package type build rule is
used. If one of these needs to be created for the target package, consult the
reference [page][pkg-doc] for this. Some build rule types are actually
extensions of the package rule type, for example [`flutter_app`][flutter-gni]
extends the package type.

Once an appropriate build rule is available the target package can be
re-generated by running `fx build`.

## Connecting host and target

The Fuchsia source contains a simple HTTP server that serves static files. The
build generates and serves a [TUF][TUF-home] file tree.

The update agent on the target does not initially know where to look for
updates. To connect the agent on the target to the HTTP server running on the
development host, it must be told the IP address of the development host.
The host HTTP server is started and the update agent is configured by calling
`fx serve -v` or `fx serve-updates -v`.  `fx serve` will run both the bootserver
and the update server and is often what people use. `fx serve-updates` runs just
the update server. In both cases, `-v` is recommended because the command will
print more output, which may assist with debugging. If the host connects
successfully to the target you will see the message `Ready to push packages!` in
the shell on your host.

The update agent on the target will remain configured until it is repaved or
persistent data is lost. The host will attempt to reconfigure the update agent
when the target is rebooted.

## Triggering package updates

Packages in Fuchsia are not "installed", they are cached on an as needed
basis. There are two collections of packages on a Fuchsia system:

* **base** The `base` package set is a group of software critical to proper
  system function that must remain congruent. The Fuchsia build system
  assigns packages to `base` when they are provided to `fx set` using the
  `--with-base` flag.
  This set of software can only be updated by performing a whole system update,
  typically referred to as OTA, described below. This is updated using `fx ota`.

* **ephemeral software** Packages included in the `cache` or `universe` package
  set are ephemeral software, with updates delivered on demand. The Fuchsia
  build system assigns packages to `universe` when they are provided to `fx set`
  using the `--with` flag.
  Ephemeral software updates to the latest available version whenever the
  package URL is resolved.

## Triggering an OTA

Sometimes there may be many packages changed or the kernel may change or there
may be changes in the system package. To get kernel changes or changes in the
system package an OTA or [pave][paver] is *required* as **base** packages are
immutable for the runtime of a system. An OTA update will usually be faster
than paving or flashing the device.

The command `fx ota` asks the target device to perform an update from any of
the update sources available to it. To OTA update a build made on the dev host to
a  target on the same LAN, first build the system you want. If `fx serve [-v]`
isn't already running, start it so the target can use the development host as an
update source. The `-v` option will show more information about the files the
target is requesting from the host. If the `-v` flag was used there should
be a flurry of output as the target retrieves all the new files. Following
completion of the OTA the device will reboot.

## Just the commands

  * `fx serve -v` (to run the update server for both build-push and ota)
    * `fx serve-updates -v` (to run only the update server, not the bootserver)
  * `fx run <component-url>` (each time a change is made you want to run)
  * `fx test <component-url>` (to build and run tests)
  * `fx ota` (to trigger a full system update and reboot)

## Issues and considerations

### You can fill up your disk

Every update pushed is stored in the content-addressed file system, blobfs.
Following a reboot the updated packages may not be available because the index
that locates them in blobfs is only held in RAM. The system currently does not
garbage collect inaccessible or no-longer-used packages (having garbage to
collect is a recent innovation!), but will eventually.

The command `fx gc` will reboot the target device and then evict all old
ephemeral software from the device, freeing up space.

### Restarting without rebooting

If the package being updated hosts a service managed by Fuchsia that service
may need to be restarted. Rebooting is undesirable both because it is slow and
because the package will revert to the version paved on the device. Typically
a user can terminate one or more running components on the system, either by
asking the component to terminate gracefully, or by forceufully stopping the
component using `fx shell killall <component-name>`. Upon reconnection to the
component services, or by invocation via `fx run` or `fx test`, new versions
available in the package server will be cached before launch.

### Packaging code outside the Fuchsia tree

Packaging and pushing code that lives outside the Fuchsia tree is possible, but
will require more work. The Fuchsia package format is quite simple. It consists
of a metadata file describing the package contents, which is described in more
detail in the [Fuchsia package][pkg-struct] documentation. The metadata file is
added to a TUF file tree and each of the contents are named after their Merkle
root hash and put in a directory at the root of the TUF file tree called 'blobs'.

[pkg-struct]: /src/sys/pkg/bin/pm/README.md#structure-of-a-fuchsia-package "Package structure"
[TUF-home]: https://theupdateframework.github.io "TUF Homepage"
[pkg-doc]: /docs/development/build/build_system/fuchsia_build_system_overview.md "Build overview"
[flutter-gni]: https://fuchsia.googlesource.com/topaz/+/HEAD/runtime/flutter_runner/flutter_app.gni "Flutter GN build template"
[paver]: /docs/development/hardware/paving.md "Fuchsia paver"
[OTA]: #triggering-an-ota "Triggering an OTA"
