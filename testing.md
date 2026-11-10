# Testing

## Built-in tests
Some of the functionality is covered by tests implemented in Rust.
These tests are run via cargo:
```sh
cargo test
```

## Processing configuration wakeups

Run the in-memory processing regressions with `cargo test --lib processing::tests`.
They exercise configuration updates with idle capture and with a full bounded
playback queue, for both pipeline rebuilds and parameter updates. They also
check old-output retirement, preserved backpressure, and the first candidate's
use of the replacement pipeline. These tests do not access audio hardware or
qualify target timing.

The processing thread commits an update when it selects the configuration event.
An audio event selected first may still use the old pipeline; enqueueing a
configuration or reporting it as active in the supervisor is not a processing
commit acknowledgement. At commit, any pending Armed request is rejected without
a candidate before admission and reconstruction. Installation retires old output
stamps and qualification. The one pending output retains its original stamp and
must be handed to playback before another capture chunk is consumed; playback
rejects the retired stamp. Pipeline construction and parameter updates remain
synchronous and have no new completion-time bound.

If the configuration channel disconnects, processing disables that receive and
continues using the installed pipeline, including normal control service. Once
end-of-stream is pending, configuration updates are no longer accepted; control
service continues until playback accepts the terminal message or disconnects.

## Config update tests
A set of tests for testing that changing the running configuration works correctly
is implemeted as a Python test script.
This requires the Python packages `pytest` and `pycamilladsp` to run.

Some tests trigger a config reload by sending SIGHUP to the camilladsp process.
This is not available on Windows.
It uses the `pgrep` command for getting the PID of the running camilladsp process,
and it assumes that only one camilladsp instance is running.

To run the tests, prepare four valid config files, named "conf1.yml" to "conf4.yml".
Example files are available in `testscripts/config_load_test`.
Place the new config files in that folder.

Start camilladsp in wait mode, with the websocket server listening on port 1234:
```sh
camilladsp -w -v -p1234
```

Now start the tests:
```sh
cd testscripts/config_load_test
pytest -v
```

A complete run takes a couple of minutes.

# Benchmarks

There are benchmarks to monitor the performance of some filters.
These use the `criterion` framework.
Run them with cargo:
```sh
cargo bench
```

## FFT Convolution Kernel Benchmarks

Micro-benchmarks for the complex-multiply kernels are available.
They compare scalar against NEON on `aarch64` and AVX+FMA on `x86_64`.
The `bench` feature flag is required to expose the kernel functions:
```sh
cargo bench --features bench --bench fftconv_kernels
```
