// Copyright 2017 Pants project contributors (see CONTRIBUTORS.md).
// Licensed under the Apache License, Version 2.0 (see LICENSE).

#![deny(warnings)]
// Enable all clippy lints except for many of the pedantic ones. It's a shame this needs to be copied and pasted across crates, but there doesn't appear to be a way to include inner attributes from a common source.
#![deny(
  clippy::all,
  clippy::default_trait_access,
  clippy::expl_impl_clone_on_copy,
  clippy::if_not_else,
  clippy::needless_continue,
  clippy::single_match_else,
  clippy::unseparated_literal_suffix,
  clippy::used_underscore_binding
)]
// It is often more clear to show that nothing is being moved.
#![allow(clippy::match_ref_pats)]
// Subjective style.
#![allow(
  clippy::len_without_is_empty,
  clippy::redundant_field_names,
  clippy::too_many_arguments
)]
// Default isn't as big a deal as people seem to think it is.
#![allow(clippy::new_without_default, clippy::new_ret_no_self)]
// Arc<Mutex> can be more clear than needing to grok Orderings:
#![allow(clippy::mutex_atomic)]

use cbindgen;
use cc;

/*

N.B. This build script is invoked by `cargo` by way of this configuration
in our Cargo.toml:

    [project]
    ...
    build = "src/cffi_build.rs"

Within, we use the `gcc` crate to compile the CFFI C sources (`native_engine.c`)
generated by `bootstrap.sh` into a (private) static lib (`libnative_engine_ffi.a`),
which then gets linked into the final `cargo build` product (the native engine binary).
This process mixes the Python module initialization function and other symbols into the
native engine binary, allowing us to address it both as an importable python module
(`from _native_engine import X`) as well as a C library (`ffi.dlopen(native_engine.so)`).

*/

use std::env;
use std::fs;
use std::io::{self, Read};
use std::path::Path;
use std::process::{exit, Command};

use build_utils::BuildRoot;

#[derive(Debug)]
enum CffiBuildError {
  IoError(io::Error),
  EnvError(env::VarError),
  CbindgenError(cbindgen::Error),
}

impl From<env::VarError> for CffiBuildError {
  fn from(err: env::VarError) -> Self {
    CffiBuildError::EnvError(err)
  }
}

impl From<io::Error> for CffiBuildError {
  fn from(err: io::Error) -> Self {
    CffiBuildError::IoError(err)
  }
}

impl From<cbindgen::Error> for CffiBuildError {
  fn from(err: cbindgen::Error) -> Self {
    CffiBuildError::CbindgenError(err)
  }
}

// A message is printed to stderr, and the script fails, if main() results in a CffiBuildError.
fn main() -> Result<(), CffiBuildError> {
  // We depend on grpcio, which uses C++.
  // On Linux, with g++, some part of that compilation depends on
  // __gxx_personality_v0 which is present in the C++ standard library.
  // I don't know why. It shouldn't, and before grpcio 0.2.0, it didn't.
  //
  // So we need to link against the C++ standard library. Nothing under us
  // in the dependency tree appears to export this fact.
  // Ideally, we would be linking dynamically, because statically linking
  // against libstdc++ is kind of scary. But we're only doing it to pull in a
  // bogus symbol anyway, so what's the worst that can happen?
  //
  // The only way I can find to dynamically link against libstdc++ is to pass
  // `-C link-args=lstdc++` to rustc, but we can only do this from a
  // .cargo/config file, which applies that argument to every compile/link which
  // happens in a subdirectory of that directory, which isn't what we want to do.
  // So we'll statically link. Because what's the worst that can happen?
  //
  // The following do not work:
  //  * Using the link argument in Cargo.toml to specify stdc++.
  //  * Specifying `rustc-flags=-lstdc++`
  //    (which is equivalent to `-ldylib=stdc++`).
  //  * Specifying `rustc-link-lib=stdc++`
  //    (which is equivalent to `rustc-link-lib=dylib=stdc++).

  // NB: When built with Python 3, `native_engine.so` only works with a Python 3 interpreter.
  // When built with Python 2, it works with both Python 2 and Python 3.
  // So, we check to see if the under-the-hood interpreter has changed and rebuild the native engine
  // when needed.
  println!("cargo:rerun-if-env-changed=PY");

  if cfg!(target_os = "linux") {
    println!("cargo:rustc-link-lib=static=stdc++");
  }

  // Generate the scheduler.h bindings from the rust code in this crate.
  let bindings_config_path = Path::new("cbindgen.toml");
  mark_for_change_detection(&bindings_config_path);

  let scheduler_file_path = Path::new("src/cffi/scheduler.h");
  let crate_dir = env::var("CARGO_MANIFEST_DIR")?;
  cbindgen::generate(crate_dir)?.write_to_file(scheduler_file_path);

  // Generate the cffi c sources.
  let build_root = BuildRoot::find()?;
  let cffi_bootstrapper = build_root.join("build-support/bin/native/bootstrap_cffi.sh");
  mark_for_change_detection(&cffi_bootstrapper);

  mark_for_change_detection(&build_root.join("src/python/pants/engine/native.py"));

  // N.B. The filename of this source code - at generation time - must line up 1:1 with the
  // python import name, as python keys the initialization function name off of the import name.
  let cffi_dir = Path::new("src/cffi");
  let c_path = cffi_dir.join("native_engine.c");
  mark_for_change_detection(&c_path);
  let env_script_path = cffi_dir.join("native_engine.cflags");
  mark_for_change_detection(&env_script_path);

  let result = Command::new(&cffi_bootstrapper)
    .arg(cffi_dir)
    .arg(scheduler_file_path)
    .status()?;
  if !result.success() {
    let exit_code = result.code();
    eprintln!(
      "Execution of {:?} failed with exit code {:?}",
      cffi_bootstrapper, exit_code
    );
    exit(exit_code.unwrap_or(1));
  }

  // Now compile the cffi c sources.
  let mut config = cc::Build::new();

  let cfg_path = c_path.to_str().unwrap();
  config.file(cfg_path);
  for flag in make_flags(&env_script_path)? {
    config.flag(flag.as_str());
  }

  // cffi generates missing field initializers :(
  config.flag("-Wno-missing-field-initializers");

  config.compile("libnative_engine_ffi.a");

  Ok(())
}

fn mark_for_change_detection(path: &Path) {
  // Restrict re-compilation check to just our input files.
  // See: http://doc.crates.io/build-script.html#outputs-of-the-build-script
  println!("cargo:rerun-if-changed={}", path.to_str().unwrap());
}

fn make_flags(env_script_path: &Path) -> Result<Vec<String>, io::Error> {
  let mut contents = String::new();
  fs::File::open(env_script_path)?.read_to_string(&mut contents)?;
  // It would be a shame if someone were to include a space in an actual quoted value.
  // If they did that, I guess we'd need to implement shell tokenization or something.
  return Ok(
    contents
      .trim()
      .split_whitespace()
      .map(str::to_owned)
      .collect(),
  );
}
