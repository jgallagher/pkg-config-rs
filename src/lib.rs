//! A build dependency for Cargo libraries to find system artifacts through the
//! `pkg-config` utility.
//!
//! This library will shell out to `pkg-config` as part of build scripts and
//! probe the system to determine how to link to a specified library. The
//! `Config` structure serves as a method of configuring how `pkg-config` is
//! invoked in a builder style.
//!
//! A number of environment variables are available to globally configure how
//! this crate will invoke `pkg-config`:
//!
//! * `PKG_CONFIG_ALLOW_CROSS` - if this variable is not set, then `pkg-config`
//!   will automatically be disabled for all cross compiles.
//! * `FOO_NO_PKG_CONFIG` - if set, this will disable running `pkg-config` when
//!   probing for the library named `foo`.
//!
//! There are also a number of environment variables which can configure how a
//! library is linked to (dynamically vs statically). These variables control
//! whether the `--static` flag is passed. Note that this behavior can be
//! overridden by configuring explicitly on `Config`. The variables are checked
//! in the following order:
//!
//! * `FOO_STATIC` - pass `--static` for the library `foo`
//! * `FOO_DYNAMIC` - do not pass `--static` for the library `foo`
//! * `PKG_CONFIG_ALL_STATIC` - pass `--static` for all libraries
//! * `PKG_CONFIG_ALL_DYNAMIC` - do not pass `--static` for all libraries
//!
//! After running `pkg-config` all appropriate Cargo metadata will be printed on
//! stdout if the search was successful.
//!
//! # Example
//!
//! Find the system library named `foo`.
//!
//! ```no_run
//! extern crate "pkg-config" as pkg_config;
//!
//! fn main() {
//!     pkg_config::find_library("foo").unwrap();
//! }
//! ```
//!
//! Configure how library `foo` is linked to.
//!
//! ```no_run
//! extern crate "pkg-config" as pkg_config;
//!
//! fn main() {
//!     pkg_config::Config::new().statik(true).find("foo").unwrap();
//! }
//! ```

#![doc(html_root_url = "http://alexcrichton.com/pkg-config-rs")]
#![cfg_attr(test, deny(warnings))]
#![feature(convert)]

use std::ascii::AsciiExt;
use std::env;
use std::fs;
use std::path::{PathBuf, Path};
use std::process::Command;
use std::str;

pub fn target_supported() -> bool {
    env::var("HOST") == env::var("TARGET") ||
        env::var_os("PKG_CONFIG_ALLOW_CROSS").is_some()
}

#[derive(Clone)]
pub struct Config {
    statik: Option<bool>,
    atleast_version: Option<String>,
}

#[derive(Debug)]
pub struct Library {
    pub libs: Vec<String>,
    pub link_paths: Vec<PathBuf>,
    pub frameworks: Vec<String>,
    pub framework_paths: Vec<PathBuf>,
    pub include_paths: Vec<PathBuf>,
    _priv: (),
}

/// Simple shortcut for using all default options for finding a library.
pub fn find_library(name: &str) -> Result<Library, String> {
    Config::new().find(name)
}

impl Config {
    /// Creates a new set of configuration options which are all initially set
    /// to "blank".
    pub fn new() -> Config {
        Config {
            statik: None,
            atleast_version: None,
        }
    }

    /// Indicate whether the `--static` flag should be passed.
    ///
    /// This will override the inference from environment variables described in
    /// the crate documentation.
    pub fn statik(&mut self, statik: bool) -> &mut Config {
        self.statik = Some(statik);
        self
    }

    /// Indicate that the library must be at least version `vers`.
    pub fn atleast_version(&mut self, vers: &str) -> &mut Config {
        self.atleast_version = Some(vers.to_string());
        self
    }

    /// Run `pkg-config` to find the library `name`.
    ///
    /// This will use all configuration previously set to specify how
    /// `pkg-config` is run.
    pub fn find(&self, name: &str) -> Result<Library, String> {
        if env::var_os(&format!("{}_NO_PKG_CONFIG", envify(name))).is_some() {
            return Err(format!("pkg-config requested to be aborted for {}", name))
        } else if !target_supported() {
            return Err("pkg-config doesn't handle cross compilation. Use \
                        PKG_CONFIG_ALLOW_CROSS=1 to override".to_string());
        }

        let mut cmd = Command::new("pkg-config");
        let statik = self.statik.unwrap_or(infer_static(name));
        if statik {
            cmd.arg("--static");
        }
        cmd.arg("--libs").arg("--cflags")
           .env("PKG_CONFIG_ALLOW_SYSTEM_LIBS", "1");
        match self.atleast_version {
            Some(ref v) => { cmd.arg(&format!("{} >= {}", name, v)); }
            None => { cmd.arg(name); }
        }
        let out = try!(cmd.output().map_err(|e| {
            format!("failed to run `{:?}`: {}", cmd, e)
        }));
        let stdout = str::from_utf8(&out.stdout).unwrap();
        let stderr = str::from_utf8(&out.stderr).unwrap();
        if !out.status.success() {
            let mut msg = format!("`{:?}` did not exit successfully: {}", cmd,
                                  out.status);
            if stdout.len() > 0 {
                msg.push_str("\n--- stdout\n");
                msg.push_str(stdout);
            }
            if stderr.len() > 0 {
                msg.push_str("\n--- stderr\n");
                msg.push_str(stderr);
            }
            return Err(msg)
        }

        let mut ret = Library {
            libs: Vec::new(),
            link_paths: Vec::new(),
            include_paths: Vec::new(),
            frameworks: Vec::new(),
            framework_paths: Vec::new(),
            _priv: (),
        };
        let mut dirs = Vec::new();
        let parts = stdout.split(' ').filter(|l| l.len() > 2)
                          .map(|arg| (&arg[0..2], &arg[2..]))
                          .collect::<Vec<_>>();
        for &(flag, val) in parts.iter() {
            if flag == "-L" {
                println!("cargo:rustc-link-search=native={}", val);
                dirs.push(PathBuf::from(val));
                ret.link_paths.push(PathBuf::from(val));
            } else if flag == "-F" {
                println!("cargo:rustc-link-search=framework={}", val);
                ret.framework_paths.push(PathBuf::from(val));
            } else if flag == "-I" {
                ret.include_paths.push(PathBuf::from(val));
            }
        }
        for &(flag, val) in parts.iter() {
            if flag == "-l" {
                ret.libs.push(val.to_string());
                if statik && !is_system_lib(val, &dirs) {
                    println!("cargo:rustc-link-lib=static={}", val);
                } else {
                    println!("cargo:rustc-link-lib={}", val);
                }
            }
        }
        let mut iter = stdout.split(' ');
        while let Some(part) = iter.next() {
            if part != "-framework" { continue }
            if let Some(lib) = iter.next() {
                println!("cargo:rustc-link-lib=framework={}", lib);
                ret.frameworks.push(lib.to_string());
            }
        }

        Ok(ret)
    }
}

fn infer_static(name: &str) -> bool {
    let name = envify(name);
    if env::var_os(&format!("{}_STATIC", name)).is_some() {
        true
    } else if env::var_os(&format!("{}_DYNAMIC", name)).is_some() {
        false
    } else if env::var_os("PKG_CONFIG_ALL_STATIC").is_some() {
        true
    } else if env::var_os("PKG_CONFIG_ALL_DYNAMIC").is_some() {
        false
    } else {
        false
    }
}

fn envify(name: &str) -> String {
    name.chars().map(|c| c.to_ascii_uppercase()).map(|c| if c == '-' {'_'} else {c})
        .collect()
}

fn is_system_lib(name: &str, dirs: &[PathBuf]) -> bool {
    let libname = format!("lib{}.a", name);
    let root = Path::new("/usr");
    !dirs.iter().any(|d| {
        !d.starts_with(root) && fs::metadata(&d.join(&libname)).is_ok()
    })
}
