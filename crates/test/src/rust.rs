use crate::config::StringList;
use crate::{Compile, LanguageMethods, Runner, Verify};
use anyhow::{Context, Result, bail};
use clap::Parser;
use heck::ToSnakeCase;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::env;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Default, Debug, Clone, Parser)]
pub struct RustOpts {
    /// A custom `path` dependency to use for `wit-bindgen`.
    #[clap(long, conflicts_with = "rust_wit_bindgen_version", value_name = "PATH")]
    rust_wit_bindgen_path: Option<PathBuf>,

    /// A custom version to use for the `wit-bindgen` dependency.
    #[clap(long, conflicts_with = "rust_wit_bindgen_path", value_name = "X.Y.Z")]
    rust_wit_bindgen_version: Option<String>,

    /// Name of the Rust target to compile for.
    #[clap(long, default_value = "wasm32-wasip2", value_name = "TARGET")]
    rust_target: String,
}

pub struct Rust;

#[derive(Default)]
pub struct State {
    wit_bindgen_files: Vec<String>,
    futures_files: Vec<String>,
    wit_bindgen_deps: Vec<PathBuf>,
}

/// Rust-specific configuration of component files
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct RustConfig {
    /// Space-separated list or array of compiler flags to pass.
    #[serde(default)]
    rustflags: StringList,
    /// List of path to rust files to build as external crates and link to the
    /// main crate.
    #[serde(default)]
    externs: Vec<String>,
    /// Additionally build this file as a position-independent shared library
    /// and check the stream/future intrinsics that library imports, as
    /// described on `check_pic_shared_link`. The file must use at least one
    /// stream or future. That extra build forces a single codegen unit,
    /// overriding any `-Ccodegen-units` in `rustflags`.
    #[serde(default)]
    pic_shared_link: bool,
}

#[derive(Deserialize)]
#[serde(tag = "reason", rename_all = "kebab-case")]
enum CargoMessage {
    CompilerArtifact {
        target: CargoTarget,
        filenames: Vec<String>,
    },
    BuildScriptExecuted {
        linked_paths: Vec<String>,
    },
}

#[derive(Deserialize, Debug)]
struct CargoTarget {
    name: String,
}

impl LanguageMethods for Rust {
    fn display(&self) -> &str {
        "rust"
    }

    fn comment_prefix_for_test_config(&self) -> Option<&str> {
        Some("//@")
    }

    fn should_fail_verify(
        &self,
        _runner: &Runner,
        name: &str,
        _config: &crate::config::WitConfig,
        _args: &[String],
    ) -> bool {
        // Currently there's a bug with this borrowing mode which means that
        // this variant does not pass.
        if name == "wasi-http-borrowed-duplicate" || name == "more-variants.wit-borrowed-duplicate"
        {
            return true;
        }

        // Named fixed-length lists don't work with async yet.
        if name == "named-fixed-length-list.wit-async" {
            return true;
        }

        false
    }

    fn codegen_test_variants(&self) -> &[(&str, &[&str])] {
        &[
            ("borrowed", &["--ownership=borrowing"]),
            (
                "borrowed-duplicate",
                &["--ownership=borrowing-duplicate-if-necessary"],
            ),
            ("async", &["--async=all"]),
            ("no-std", &["--std-feature"]),
            ("merge-equal", &["--merge-structurally-equal-types"]),
            ("hashmap", &["--map-type=std::collections::HashMap"]),
        ]
    }

    fn default_bindgen_args(&self) -> &[&str] {
        &["--generate-all", "--format"]
    }

    fn default_bindgen_args_for_codegen(&self) -> &[&str] {
        &["--stubs"]
    }

    fn prepare(&self, runner: &mut Runner) -> Result<()> {
        let cwd = env::current_dir()?;
        let opts = &runner.opts.rust;
        let dir = cwd.join(&runner.opts.artifacts).join("rust");
        let wit_bindgen = dir.join("wit-bindgen");

        let wit_bindgen_dep = match &opts.rust_wit_bindgen_path {
            Some(path) => format!("path = {:?}", cwd.join(path)),
            None => {
                let version = opts
                    .rust_wit_bindgen_version
                    .as_deref()
                    .unwrap_or(env!("CARGO_PKG_VERSION"));
                format!("version = \"{version}\"")
            }
        };

        super::write_if_different(
            &wit_bindgen.join("Cargo.toml"),
            &format!(
                r#"
[package]
name = "tmp"

[workspace]

[dependencies]
wit-bindgen = {{ {wit_bindgen_dep}, features = ['async-spawn', 'inter-task-wakeup', 'futures-stream'] }}
futures = "0.3.31"

[lib]
path = 'lib.rs'
            "#,
            ),
        )?;
        super::write_if_different(&wit_bindgen.join("lib.rs"), "")?;

        println!("Building `wit-bindgen` from crates.io...");
        let json = runner.run_command(
            Command::new("cargo")
                .current_dir(&wit_bindgen)
                .arg("build")
                .arg("-pwit-bindgen")
                .arg("-pfutures")
                .arg("--target")
                .arg(&opts.rust_target)
                .arg("--message-format=json"),
        )?;
        let mut wit_bindgen_files = None;
        let mut futures_files = None;
        let mut wit_bindgen_deps = Vec::new();
        let mut deps_seen = HashSet::new();

        for line in json.lines() {
            let Ok(msg) = serde_json::from_str(line) else {
                continue;
            };
            match msg {
                CargoMessage::CompilerArtifact { target, filenames } => {
                    if target.name == "futures" {
                        futures_files = Some(filenames.clone());
                    } else if target.name == "wit_bindgen" {
                        wit_bindgen_files = Some(filenames.clone());
                    }
                    for file in &filenames {
                        let dir = Path::new(file).parent().unwrap();
                        if deps_seen.insert(dir.to_path_buf()) {
                            wit_bindgen_deps.push(dir.to_path_buf());
                        }
                    }
                }
                CargoMessage::BuildScriptExecuted { linked_paths } => {
                    for path in linked_paths {
                        let path = PathBuf::from(path);
                        if deps_seen.insert(path.clone()) {
                            wit_bindgen_deps.push(path);
                        }
                    }
                }
            }
        }

        runner.rust_state = Some(State {
            wit_bindgen_files: wit_bindgen_files.unwrap().into(),
            futures_files: futures_files.unwrap().into(),
            wit_bindgen_deps,
        });
        Ok(())
    }

    fn compile(&self, runner: &Runner, compile: &Compile) -> Result<()> {
        let config = compile.component.deserialize_lang_config::<RustConfig>()?;

        // If this rust target doesn't natively produce a component then place
        // the compiler output in a temporary location which is componentized
        // later on.
        let output = compile.output.with_extension("core.wasm");

        // Compile all extern crates, if any
        let mut externs = Vec::new();
        let manifest_dir = compile.component.path.parent().unwrap();

        let rustc = |path: &Path, output: &Path| {
            // Compile the main crate, passing `--extern` for all upstream crates.
            let mut cmd = runner.rustc(Edition::E2021);
            cmd.env("CARGO_MANIFEST_DIR", manifest_dir)
                .arg(path)
                .arg("-o")
                .arg(&output);
            for flag in Vec::from(config.rustflags.clone()) {
                cmd.arg(flag);
            }
            cmd
        };

        for file in config.externs.iter() {
            let file = manifest_dir.join(file);
            let stem = file.file_stem().unwrap().to_str().unwrap();
            let output = compile.artifacts_dir.join(format!("lib{stem}.rlib"));
            runner.run_command(rustc(&file, &output).arg("--crate-type=rlib"))?;
            externs.push((stem.to_string(), output));
        }

        let bindings = compile.bindings_dir.join(format!(
            "{}.rs",
            compile.component.bindgen.world.replace('-', "_")
        ));

        // Compile the main crate, passing `--extern` for all upstream crates.
        let mut cmd = rustc(&compile.component.path, &output);
        cmd.env("BINDINGS", &bindings);
        for (name, path) in externs.iter() {
            let arg = format!("--extern={name}={}", path.display());
            cmd.arg(arg);
        }
        cmd.arg("--crate-type=cdylib");
        if runner.produces_component() {
            cmd.arg("-Clink-arg=--skip-wit-component");
        }
        runner.run_command(&mut cmd)?;

        runner
            .convert_p1_to_component(&output, compile)
            .with_context(|| format!("failed to convert {output:?}"))?;

        if config.pic_shared_link {
            // A single codegen unit keeps `--emit=obj` to the one object file
            // named by `-o`.
            let object = compile.artifacts_dir.join("pic.o");
            let mut cmd = rustc(&compile.component.path, &object);
            cmd.env("BINDINGS", &bindings);
            for (name, path) in externs.iter() {
                let arg = format!("--extern={name}={}", path.display());
                cmd.arg(arg);
            }
            cmd.arg("--crate-type=cdylib")
                .arg("--emit=obj")
                .arg("-Ccodegen-units=1")
                .arg("-Crelocation-model=pic");
            runner.run_command(&mut cmd)?;

            let library = compile.artifacts_dir.join("pic.so");
            runner.link_shared(&object, &library)?;
            check_pic_shared_link(&library)
                .with_context(|| format!("unusable stream/future vtables in {library:?}"))?;
        }

        Ok(())
    }

    fn verify(&self, runner: &Runner, verify: &Verify<'_>) -> Result<()> {
        let bindings = verify
            .bindings_dir
            .join(format!("{}.rs", verify.world.to_snake_case()));
        let test_edition = |edition: Edition| -> Result<()> {
            let mut cmd = runner.rustc(edition);
            cmd.arg(&bindings)
                .arg("--crate-type=rlib")
                .arg("-o")
                .arg(verify.artifacts_dir.join("tmp"));
            runner.run_command(&mut cmd)?;
            Ok(())
        };

        test_edition(Edition::E2021)?;
        test_edition(Edition::E2024)?;

        // If bindings are generated in `#![no_std]` mode then verify that it
        // compiles as such.
        if verify.args.iter().any(|s| s == "--std-feature") {
            let no_std_root = verify.artifacts_dir.join("no_std.rs");
            super::write_if_different(
                &no_std_root,
                r#"
#![no_std]
include!(env!("BINDINGS"));

// This empty module named 'core' is here to catch module path
// conflicts with 'core' modules used in code generated by the
// wit_bindgen::generate macro.
// Ref: https://github.com/bytecodealliance/wit-bindgen/pull/568
mod core {}
                "#,
            )?;
            let mut cmd = runner.rustc(Edition::E2021);
            cmd.arg(&no_std_root)
                .env("BINDINGS", &bindings)
                .arg("--crate-type=rlib")
                .arg("-o")
                .arg(verify.artifacts_dir.join("tmp"));
            runner.run_command(&mut cmd)?;
        }
        Ok(())
    }
}

enum Edition {
    E2021,
    E2024,
}

impl Runner {
    fn rustc(&self, edition: Edition) -> Command {
        let state = self.rust_state.as_ref().unwrap();
        let opts = &self.opts.rust;
        let mut cmd = Command::new("rustc");
        cmd.arg(match edition {
            Edition::E2021 => "--edition=2021",
            Edition::E2024 => "--edition=2024",
        })
        .arg("--target")
        .arg(&opts.rust_target)
        .arg("-Dwarnings")
        .arg("-Cdebuginfo=1");
        for dep in state.wit_bindgen_deps.iter() {
            let dep = dep.display();
            if dep.to_string().contains('=') {
                cmd.arg(&format!("-L{dep}"));
            } else {
                cmd.arg(&format!("-Ldependency={dep}"));
            }
        }
        for (name, files) in [
            ("wit_bindgen", &state.wit_bindgen_files),
            ("futures", &state.futures_files),
        ] {
            for file in files {
                cmd.arg(&format!("--extern={name}={file}",));
            }
        }
        cmd
    }

    fn produces_component(&self) -> bool {
        match self.opts.rust.rust_target.as_str() {
            "wasm32-unknown-unknown" | "wasm32-wasi" | "wasm32-wasip1" => false,
            _ => true,
        }
    }

    /// Links `object` into a position-independent shared library at `output`
    /// using the `rust-lld` of the current toolchain.
    ///
    /// Only `object` is linked, so the symbols it uses from the standard
    /// library are left undefined and `--allow-undefined` turns them into
    /// imports. That keeps this link independent of whether a
    /// position-independent build of the standard library is available.
    fn link_shared(&self, object: &Path, output: &Path) -> Result<()> {
        let libdir = self.run_command(Command::new("rustc").arg("--print").arg("target-libdir"))?;
        let lld = Path::new(libdir.trim())
            .parent()
            .with_context(|| format!("failed to find the toolchain directory of {libdir:?}"))?
            .join("bin")
            .join(format!("rust-lld{}", env::consts::EXE_SUFFIX));
        self.run_command(
            Command::new(&lld)
                .arg("-flavor")
                .arg("wasm")
                .arg("-shared")
                .arg("--experimental-pic")
                .arg("--allow-undefined")
                .arg("--no-entry")
                .arg("-o")
                .arg(output)
                .arg(object),
        )?;
        Ok(())
    }
}

/// The seven intrinsics stored in a stream or future vtable, as they are
/// spelled in import names.
const VTABLE_INTRINSICS: &[&str] = &[
    "new",
    "cancel-write",
    "cancel-read",
    "drop-writable",
    "drop-readable",
    "read",
    "write",
];

/// Checks that the shared library `library` imports all seven intrinsics of
/// every stream/future vtable it contains, and that it contains at least one
/// such vtable.
///
/// An import that is only address-taken is lowered by LLVM to a `GOT.func`
/// global keyed by its mangled Rust symbol, and the canonical `(module, field)`
/// import is dropped. This happens in position-independent code, which is how
/// a shared library is built. No library exports the mangled symbol, so a
/// component linker leaves the `GOT.func` slot unresolved and calls through the
/// vtable trap. Generated bindings therefore store the address of a local
/// wrapper in the vtable and have the wrapper call the intrinsic by name, which
/// keeps the canonical import in the module.
fn check_pic_shared_link(library: &Path) -> Result<()> {
    let bytes = fs::read(library).with_context(|| format!("failed to read {library:?}"))?;

    // Keyed by the import module, the payload kind, the vtable index, and the
    // name of the function the vtable belongs to.
    let mut vtables = BTreeMap::<_, BTreeSet<_>>::new();
    let mut got_func = Vec::new();
    for payload in wasmparser::Parser::new(0).parse_all(&bytes) {
        let wasmparser::Payload::ImportSection(section) = payload? else {
            continue;
        };
        for import in section.into_imports() {
            let import = import?;
            if import.module == "GOT.func" {
                got_func.push(import.name.to_string());
            } else if let Some(((kind, index, func), intrinsic)) =
                parse_vtable_intrinsic(import.name)
            {
                vtables
                    .entry((import.module.to_string(), kind, index, func))
                    .or_default()
                    .insert(intrinsic);
            }
        }
    }

    if vtables.is_empty() {
        let mut error = String::from(
            "no stream or future intrinsic is imported by name, so no vtable in \
             this library can be linked into a component",
        );
        if !got_func.is_empty() {
            error.push_str("\nthe library imports these `GOT.func` globals instead:");
            for name in got_func.iter() {
                write!(error, "\n  {name}")?;
            }
        }
        bail!("{error}");
    }

    for ((module, kind, index, func), intrinsics) in vtables {
        let missing = VTABLE_INTRINSICS
            .iter()
            .filter(|name| !intrinsics.contains(**name))
            .copied()
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            bail!(
                "the {kind} vtable `{index}` of `{module}#{func}` is missing imports \
                 of these intrinsics: {}",
                missing.join(", "),
            );
        }
    }

    Ok(())
}

/// Splits an import name such as `[async-lower][stream-read-0]read-stream` into
/// the vtable it belongs to, `("stream", 0, "read-stream")`, and the intrinsic
/// within that vtable, `"read"`. Returns `None` for all other import names.
fn parse_vtable_intrinsic(name: &str) -> Option<((&str, u32, &str), &str)> {
    let name = name.strip_prefix("[async-lower]").unwrap_or(name);
    let (bracketed, func) = name.strip_prefix('[')?.split_once(']')?;
    let (kind, rest) = bracketed.split_once('-')?;
    if kind != "stream" && kind != "future" {
        return None;
    }
    let (intrinsic, index) = rest.rsplit_once('-')?;
    if !VTABLE_INTRINSICS.contains(&intrinsic) {
        return None;
    }
    Some(((kind, index.parse().ok()?, func), intrinsic))
}
