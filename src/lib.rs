#![deny(rust_2018_idioms)]
#![deny(unused_crate_dependencies)]

use futures::{future::BoxFuture, FutureExt};
use snafu::prelude::*;
use std::{collections::BTreeMap, future::Future, path::PathBuf, process::ExitCode};
use tempfile::TempDir;
use tokio::{fs, process::Command};

type BoxError = Box<dyn snafu::Error + Send + Sync + 'static>;

pub async fn test_conformance<R: Registry + Send + Sync + 'static>() -> ExitCode {
    let name_length_1 = wrap_test::<R, _>(|a, b| name_length_1(a, b).boxed()).await;
    let name_length_2 = wrap_test::<R, _>(|a, b| name_length_2(a, b).boxed()).await;
    let name_length_3 = wrap_test::<R, _>(|a, b| name_length_3(a, b).boxed()).await;
    let name_length_4 = wrap_test::<R, _>(|a, b| name_length_4(a, b).boxed()).await;
    let multiple_sibling_dependencies =
        wrap_test::<R, _>(|a, b| multiple_sibling_dependencies(a, b).boxed()).await;
    let multiple_hierarchical_dependencies =
        wrap_test::<R, _>(|a, b| multiple_hierarchical_dependencies(a, b).boxed()).await;

    let mut exit_code = ExitCode::SUCCESS;

    for test in [
        name_length_1,
        name_length_2,
        name_length_3,
        name_length_4,
        multiple_sibling_dependencies,
        multiple_hierarchical_dependencies,
    ] {
        if let Err(e) = test {
            eprintln!("{}", snafu::Report::from_error(e));
            exit_code = ExitCode::FAILURE;
        }
    }

    exit_code
}

async fn wrap_test<R, F>(f: F) -> Result<(), TestError>
where
    R: Registry,
    F: for<'a> FnOnce(&'a ScratchSpace, &'a mut R) -> BoxFuture<'a, Result<(), BoxError>>,
{
    use test_error::*;

    let scratch = ScratchSpace::new().await.context(ScratchSnafu)?;
    let mut registry = R::start(&scratch.registry_path)
        .await
        .boxed()
        .context(RegistryStartSnafu)?;

    match f(&scratch, &mut registry).await.context(FailureSnafu) {
        Ok(it) => it,
        Err(err) => {
            let scratch = scratch.leave_it();
            eprintln!("Artifacts left in {}", scratch.display());
            return Err(err);
        }
    };

    registry
        .shutdown()
        .await
        .boxed()
        .context(RegistryShutdownSnafu)?;

    Ok(())
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum TestError {
    #[snafu(display("Could not create the registry scratch space"))]
    Scratch { source: ScratchSpaceError },

    #[snafu(display("Could not start the registry"))]
    RegistryStart { source: BoxError },

    #[snafu(display("The test failed"))]
    Failure { source: BoxError },

    #[snafu(display("Could not shut down the registry"))]
    RegistryShutdown { source: BoxError },
}

async fn name_length_1(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> Result<(), BoxError> {
    parameterized_name(scratch, registry, "a").await
}

async fn name_length_2(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> Result<(), BoxError> {
    parameterized_name(scratch, registry, "ab").await
}

async fn name_length_3(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> Result<(), BoxError> {
    parameterized_name(scratch, registry, "abc").await
}

async fn name_length_4(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> Result<(), BoxError> {
    parameterized_name(scratch, registry, "abcd").await
}

async fn parameterized_name(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
    name: &str,
) -> Result<(), BoxError> {
    let library_crate = Crate::new(name, "0.1.0")
        .lib_rs("pub fn add(a: u8, b: u8) -> u8 { a + b }")
        .create_in(scratch)
        .await?;

    let registry_url = registry.registry_url().await;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry("mine", &registry_url)
        .add_dependency("mine", &library_crate)
        .main_rs(format!(
            "fn main() {{ assert_eq!(3, {crate_name}::add(1, 2)); }}",
            crate_name = library_crate.name,
        ))
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn multiple_sibling_dependencies(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> Result<(), BoxError> {
    let left_crate = Crate::new("left", "0.1.0")
        .lib_rs("pub fn add(a: u8, b: u8) -> u8 { a + b }")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&left_crate).await?;

    let right_crate = Crate::new("right", "0.2.0")
        .lib_rs("pub fn mul(a: u8, b: u8) -> u8 { a * b }")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&right_crate).await?;

    let registry_url = registry.registry_url().await;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry("mine", &registry_url)
        .add_dependency("mine", &left_crate)
        .add_dependency("mine", &right_crate)
        .main_rs("fn main() { assert_eq!(7, left::add(1, right::mul(2, 3))); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn multiple_hierarchical_dependencies(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> Result<(), BoxError> {
    let registry_url = registry.registry_url().await;

    let two_away_crate = Crate::new("two", "0.1.0")
        .lib_rs("pub fn add(a: u8, b: u8) -> u8 { a + b }")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&two_away_crate).await?;

    let one_away_crate = Crate::new("one", "0.2.0")
        .add_registry("mine", &registry_url)
        .add_dependency("mine", &two_away_crate)
        .lib_rs("pub fn triple(a: u8) -> u8 { two::add(a, two::add(a, a)) }")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&one_away_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry("mine", &registry_url)
        .add_dependency("mine", &one_away_crate)
        .main_rs("fn main() { assert_eq!(9, one::triple(3)); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

struct ScratchSpace {
    #[allow(unused)]
    root: TempDir,
    crates_path: PathBuf,
    registry_path: PathBuf,
}

impl ScratchSpace {
    async fn new() -> Result<Self, ScratchSpaceError> {
        use scratch_space_error::*;

        let root = TempDir::new().context(RootSnafu)?;

        let crates_path = root.path().join("crates");
        fs::create_dir_all(&crates_path)
            .await
            .context(CratesCreateSnafu { path: &crates_path })?;

        let registry_path = root.path().join("registry");
        fs::create_dir_all(&registry_path)
            .await
            .context(RegistryCreateSnafu {
                path: &registry_path,
            })?;

        Ok(Self {
            root,
            crates_path,
            registry_path,
        })
    }

    fn leave_it(self) -> PathBuf {
        self.root.into_path()
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum ScratchSpaceError {
    #[snafu(display("Could not create the scratch space root"))]
    Root { source: std::io::Error },

    #[snafu(display("Could not create the crates directory at {}", path.display()))]
    CratesCreate {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not create the registry directory at {}", path.display()))]
    RegistryCreate {
        source: std::io::Error,
        path: PathBuf,
    },
}

pub trait Registry: Sized {
    type Error: snafu::Error + Send + Sync + 'static;

    fn start(path: impl Into<PathBuf>) -> impl Future<Output = Result<Self, Self::Error>>;

    fn registry_url(&self) -> impl Future<Output = String> + Send;

    fn publish_crate(
        &mut self,
        crate_: &CreatedCrate,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn shutdown(self) -> impl Future<Output = Result<(), Self::Error>>;
}

#[derive(Debug)]
struct Crate {
    cargo_toml: cargo_toml::Root,
    src: BTreeMap<PathBuf, String>,
    dotcargo_config: Option<dotcargo_config::Root>,
}

impl Crate {
    fn new(name: impl Into<String>, version: impl Into<String>) -> Crate {
        Self {
            cargo_toml: cargo_toml::Root {
                package: cargo_toml::Package {
                    name: name.into(),
                    version: version.into(),
                },
                dependencies: Default::default(),
            },
            dotcargo_config: Default::default(),
            src: Default::default(),
        }
    }

    fn lib_rs(mut self, contents: impl Into<String>) -> Self {
        self.src.insert("lib.rs".into(), contents.into());
        self
    }

    fn main_rs(mut self, contents: impl Into<String>) -> Self {
        self.src.insert("main.rs".into(), contents.into());
        self
    }

    fn add_registry(mut self, name: impl Into<String>, url: impl Into<String>) -> Self {
        let dotcargo_config = self.dotcargo_config.get_or_insert_with(Default::default);
        dotcargo_config
            .registries
            .insert(name.into(), dotcargo_config::Registry { index: url.into() });
        self
    }

    fn add_dependency(mut self, registry: impl Into<String>, crate_: &CreatedCrate) -> Self {
        self.cargo_toml.dependencies.insert(
            crate_.name.clone(),
            cargo_toml::Dependency {
                version: crate_.version.clone(),
                registry: registry.into(),
            },
        );
        self
    }

    async fn create_in(self, scratch: &ScratchSpace) -> Result<CreatedCrate, CreateCrateError> {
        use create_crate_error::*;

        let crate_path = scratch.crates_path.join(&self.cargo_toml.package.name);
        fs::create_dir_all(&crate_path)
            .await
            .context(CrateCreateSnafu { path: &crate_path })?;

        let cargo_toml =
            toml::to_string_pretty(&self.cargo_toml).context(CargoTomlSerializeSnafu)?;
        let cargo_toml_path = crate_path.join("Cargo.toml");
        fs::write(&cargo_toml_path, cargo_toml)
            .await
            .context(CargoTomlWriteSnafu {
                path: cargo_toml_path,
            })?;

        if let Some(dotcargo_config) = &self.dotcargo_config {
            let dotcargo_path = crate_path.join(".cargo");
            fs::create_dir_all(&dotcargo_path)
                .await
                .context(DotCargoCreateSnafu {
                    path: &dotcargo_path,
                })?;

            let dotcargo_config_path = dotcargo_path.join("config.toml");
            let dotcargo_config =
                toml::to_string_pretty(&dotcargo_config).context(DotCargoConfigSerializeSnafu)?;
            fs::write(&dotcargo_config_path, dotcargo_config)
                .await
                .context(DotCargoConfigWriteSnafu {
                    path: dotcargo_config_path,
                })?;
        }

        if !self.src.is_empty() {
            let src_path = crate_path.join("src");
            fs::create_dir_all(&src_path)
                .await
                .context(SrcCreateSnafu { path: &src_path })?;

            for (name, contents) in &self.src {
                let src_path = src_path.join(name);
                fs::write(&src_path, contents)
                    .await
                    .context(SourceCodeWriteSnafu { path: src_path })?
            }
        }

        let Self { cargo_toml, .. } = self;
        let cargo_toml::Root { package, .. } = cargo_toml;
        let cargo_toml::Package { name, version } = package;

        Ok(CreatedCrate {
            directory: crate_path,
            name,
            version,
        })
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum CreateCrateError {
    #[snafu(display("Could not create the crate directory {}", path.display()))]
    CrateCreate {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not serialize Cargo.toml"))]
    CargoTomlSerialize { source: toml::ser::Error },

    #[snafu(display("Could not write Cargo.toml to {}", path.display()))]
    CargoTomlWrite {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not create the .cargo directory {}", path.display()))]
    DotCargoCreate {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not serialize .cargo/config.toml"))]
    DotCargoConfigSerialize { source: toml::ser::Error },

    #[snafu(display("Could not write .cargo/config.toml to {}", path.display()))]
    DotCargoConfigWrite {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not create the src directory {}", path.display()))]
    SrcCreate {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not write the source file {}", path.display()))]
    SourceCodeWrite {
        source: std::io::Error,
        path: PathBuf,
    },
}

#[derive(Debug)]
pub struct CreatedCrate {
    directory: PathBuf,
    name: String,
    version: String,
}

impl CreatedCrate {
    pub async fn package(&self) -> Result<PathBuf, PackageError> {
        use package_error::*;

        self.cargo_command()
            .arg("package")
            .expect_success()
            .await
            .context(ExecutionSnafu)?;

        let package_path = self.package_path();
        ensure!(
            package_path.exists(),
            MissingSnafu {
                path: &package_path
            }
        );

        Ok(package_path)
    }

    async fn run(&self) -> Result<(), RunError> {
        use run_error::*;

        self.cargo_command()
            .arg("run")
            .expect_success()
            .await
            .context(ExecutionSnafu)
    }

    fn cargo_command(&self) -> Command {
        let mut cmd = Command::new("cargo");

        cmd.current_dir(&self.directory)
            .env("RUSTUP_TOOLCHAIN", "stable")
            .kill_on_drop(true);

        cmd
    }

    fn package_path(&self) -> PathBuf {
        let Self {
            directory,
            name,
            version,
        } = self;

        let mut package_path = directory.join("target");
        package_path.push("package");
        package_path.push(format!("{name}-{version}.crate"));
        package_path
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum PackageError {
    #[snafu(display("Could not run `cargo package`"))]
    Execution { source: CommandError },

    #[snafu(display("The package file `{}` was not created", path.display()))]
    Missing { path: PathBuf },
}

#[derive(Debug, Snafu)]
#[snafu(module)]
enum RunError {
    #[snafu(display("Could not run `cargo run`"))]
    Execution { source: CommandError },
}

mod dotcargo_config {
    use serde::Serialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Default, Serialize)]
    pub struct Root {
        pub registries: BTreeMap<String, Registry>,
    }

    #[derive(Debug, Serialize)]
    pub struct Registry {
        pub index: String,
    }
}

mod cargo_toml {
    use serde::Serialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Serialize)]
    pub struct Root {
        pub package: Package,
        pub dependencies: BTreeMap<String, Dependency>,
    }

    #[derive(Debug, Serialize)]
    pub struct Package {
        pub name: String,
        pub version: String,
    }

    #[derive(Debug, Serialize)]
    pub struct Dependency {
        pub version: String,
        pub registry: String,
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum CommandError {
    #[snafu(transparent)]
    Output { source: std::io::Error },

    #[snafu(display("The command failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"))]
    ErrorExit { stdout: String, stderr: String },
}

#[allow(async_fn_in_trait)]
pub trait CommandExt {
    async fn expect_success(&mut self) -> Result<(), CommandError>;
}

impl CommandExt for Command {
    async fn expect_success(&mut self) -> Result<(), CommandError> {
        use command_error::*;

        let out = self.output().await?;
        ensure!(out.status.success(), {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            ErrorExitSnafu { stdout, stderr }
        });

        Ok(())
    }
}
