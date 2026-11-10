> **Patched copy for Deltos.** This is xattr 0.2.3 from crates.io, used by GPUI's HTTP client
> through `zed-async-tar`. Its one change: on Linux and Android, `ENOATTR` is defined as
> `libc::ENODATA`, because libc 0.2.191 no longer exports `ENOATTR` for those targets and the
> unmodified crate fails to compile. The workspace `Cargo.toml` applies it with
> `[patch.crates-io]`. It can go once GPUI depends on a release of xattr with the same fix.
> The tests were removed; the code is otherwise unchanged and keeps its MIT/Apache-2.0 licenses.

xattr
=====

[![Build Status](https://travis-ci.org/Stebalien/xattr.svg?branch=master)](https://travis-ci.org/Stebalien/xattr)
[![Build Status](https://api.cirrus-ci.com/github/Stebalien/xattr.svg)](https://cirrus-ci.com/github/Stebalien/xattr)

A small library for setting, getting, and listing extended attributes.

Supported Platforms: Android, Linux, MacOS, FreeBSD, and NetBSD.

API Documentation: https://stebalien.github.com/xattr/xattr/

Unsupported Platforms
--------------------------

This library includes no-op support for unsupported platforms. That is, it will
build on *all* platforms but always fail on unsupported platforms.

1. You can turn this off by disabling the default `unsupported` feature. If you
   do so, this library will fail to compile on unsupported platforms.
2. Alternatively, you can detect unsupported platforms at runtime by checking
   the `xattr::SUPPORTED_PLATFORM` boolean.
