# Third-Party Attributions

## sandbox-runtime (Apache-2.0)
The macOS Seatbelt profile allow-set design (mach-lookup of 13 system services,
ipc-posix-shm/sem for mmap, sysctl-read, file-ioctl on /dev/null|zero|random|
urandom|dtracehelper|tty, iokit-get-properties, system-socket AF_SYSTEM) in
`src/lib.rs::render_profile` is informed by Anthropic's sandbox-runtime
(https://github.com/anthropics/sandbox-runtime), Apache-2.0.

houyicoder-sandbox is an independent Rust reimplementation. No source code was
copied; only the idea-level allow-set (which mach services and /dev devices
dyld touches during process start) was informed by sandbox-runtime's
`src/sandbox/macos-sandbox-utils.ts`.
