# run-test-suite

Reviewed on: 2022-01-05

This folder contains the run_test_suite *library* and run-test-suite *binary*.

* The run-test-suite *binary* is a command line tool run on a Fuchsia device that runs
tests which implement `fuchsia.test.Suite`.
* `ffx test` is a plugin for [ffx][ffx] that runs tests which implement
`fuchsia.test.Suite`. It is located in the [ffx plugins][ffx-plugins] folder. See the
[ffx plugin development guide][ffx-dev-guide] for details on how to develop ffx test.
* The run_test_suite *library* contains common code used for both the run-test-suite
*library* and `ffx test`.

Run test suite runs tests which implement `fuchsia.test.Suite` and displays result.
It will exit with code 0 if tests passes else with code 1.

## Building

run-test-suite should be included test build of Fuchsia, but if missing
can be added to builds by including `--with //src/sys/run_test_suite` to the
`fx set` invocation.

## Running

### run-test-suite

```
$ fx shell run run-test-suite <v2_test_component_url>
```

## Testing

Tests for the run-test-suite *library* and *binary* can be added to builds by
including `--with //src/sys/run_test_suite:tests` to the `fx set` invocation.

Unit tests can be run with

```
$ fx test run-test-suite-unit-tests
```

Integration tests for this project are available in the `tests` folder. They can
be run with

```
$ fx test run_test_suite_integration_tests
```

## Source layout

The run-test-suite *library* is located in the `src` directory. The top entry
point for the library is `src/lib.rs`.

The run-test-suite *library* is organized into two sections:
 * An executor section that is responsible for scheduling tests and streaming
 results and artifacts using the `fuchsia.test.manager` protocols. This
 comprises the files contained directly under `src`.
 * A reporter portion responsible for recording results and artifacts to
 various output formats, such as to human readable stdout and machine readable
 [structured output][structured-output]. This is comprised of the output module
 contained in `src/output`.

The entry point for the run-test-suite *binary* is located in `src/main.rs`.

[ffx]: /docs/development/tools/ffx/overview.md
[ffx-dev-guide]: /docs/development/tools/development/plugins.md
[ffx-plugins]: /src/developer/ffx/plugins/test
[structured-output]: /src/sys/run_test_suite/directory/README.md
