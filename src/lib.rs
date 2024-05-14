#![deny(rust_2018_idioms)]
#![deny(unused_crate_dependencies)]

use base64::Engine;
use futures::{future::BoxFuture, stream, FutureExt, StreamExt};
use regex::Regex;
use snafu::prelude::*;
use std::{
    collections::BTreeMap,
    future::Future,
    io,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    process::{self, ExitCode},
};
use tempfile::TempDir;
use tokio::{fs, process::Command};

use definition::{Dependency2, DependencyDefinition, RegistryDefinition};

type BoxError = Box<dyn snafu::Error + Send + Sync + 'static>;
type BoxResult<T = ()> = Result<T, BoxError>;

type TestFunction<R> =
    Box<dyn for<'a> FnOnce(&'a ScratchSpace, &'a mut R) -> BoxFuture<'a, BoxResult>>;

struct TestDefinition<R: Registry + 'static> {
    name: &'static str,
    setup: Box<dyn FnOnce() -> R::Builder>,
    f: TestFunction<R>,
}

#[derive(Debug)]
struct TestResult {
    name: &'static str,
    result: TestResultKind,
}

#[derive(Debug)]
enum TestResultKind {
    Success,
    Failure(TestError),
    Skipped,
}

impl TestResultKind {
    fn code(&self) -> &'static str {
        match self {
            Self::Success => "PASS",
            Self::Failure(_) => "FAIL",
            Self::Skipped => "SKIP",
        }
    }
}

impl From<Result<(), TestError>> for TestResultKind {
    fn from(value: Result<(), TestError>) -> Self {
        match value {
            Ok(()) => TestResultKind::Success,
            Err(e) => TestResultKind::Failure(e),
        }
    }
}

pub async fn test_conformance<R: Registry + Send + Sync + 'static>(
    mut args: impl Iterator<Item = String>,
) -> ExitCode {
    let selected_tests = match args.nth(1) {
        Some(pattern) => Regex::new(&pattern),
        None => Regex::new(".*"),
    }
    .unwrap();

    macro_rules! tests {
        ($(($setup:expr, $name:ident)),* $(,)?) => {
            [
                $(
                    TestDefinition {
                        name: stringify!($name),
                        setup: Box::new($setup),
                        f: Box::new(|a, b: &mut R| $name(a, b).boxed()),
                    }
                ,)*
            ]
        };
    }

    let to_run = tests![
        (Default::default, name_length_1),
        (Default::default, name_length_2),
        (Default::default, name_length_3),
        (Default::default, name_length_4),
        (Default::default, multiple_sibling_dependencies),
        (Default::default, multiple_hierarchical_dependencies),
        (Default::default, cross_registry_dependencies),
        (Default::default, crates_io_dependencies_unify),
        (Default::default, multiple_versions),
        (Default::default, conflicting_links),
        (Default::default, minimum_version),
        (Default::default, optional_dependency_unused),
        (Default::default, optional_dependency_used_implicit),
        (Default::default, optional_dependency_used_implicit_renamed),
        (Default::default, optional_dependency_used_explicit),
        (Default::default, platform_specific_dependency_unused),
        (Default::default, platform_specific_dependency_used),
        (Default::default, build_only_dependency),
        (authorization_required_setup, authorization_required),
    ];

    let tests = to_run.into_iter().map(|t| {
        let should_run = selected_tests.is_match(t.name);

        async move {
            let TestDefinition { name, setup, f } = t;

            let result = if should_run {
                wrap_test::<R, _, _>(setup, move |a, b| f(a, b))
                    .await
                    .into()
            } else {
                TestResultKind::Skipped
            };

            TestResult { name, result }
        }
    });

    let running = stream::iter(tests).buffer_unordered(4);

    let complete = running
        .inspect(|t| {
            let TestResult { name, result } = t;

            let code = result.code();
            eprintln!("{code}: {name}");
        })
        .collect::<Vec<_>>()
        .await;

    let mut exit_code = ExitCode::SUCCESS;

    for t in complete {
        let TestResult { name, result } = t;
        if let TestResultKind::Failure(e) = result {
            eprintln!("=== {name}");
            eprintln!("{}", snafu::Report::from_error(&e));

            if let Some(scratch_space) = e.scratch_space() {
                eprintln!("Artifacts left in {}", scratch_space.display());
            }

            exit_code = ExitCode::FAILURE;
        }
    }

    exit_code
}

async fn wrap_test<R, S, F>(setup: S, f: F) -> Result<(), TestError>
where
    R: Registry,
    S: FnOnce() -> R::Builder,
    F: for<'a> FnOnce(&'a ScratchSpace, &'a mut R) -> BoxFuture<'a, BoxResult>,
{
    use test_error::*;

    let scratch = ScratchSpace::new().await.context(ScratchSnafu)?;

    let builder = setup();

    let mut registry = builder
        .start(&scratch.registry_path)
        .await
        .boxed()
        .context(RegistryStartSnafu)?;

    let running = f(&scratch, &mut registry);
    let running = AssertUnwindSafe(running).catch_unwind();

    match running.await {
        Ok(Ok(it)) => it,

        Err(err) => {
            let scratch_space = scratch.leave_it();

            let text = match err.downcast::<String>() {
                Ok(text) => *text,
                Err(_) => "Unknown error".to_owned(),
            };

            return AssertSnafu {
                text,
                scratch_space,
            }
            .fail();
        }

        Ok(Err(err)) => {
            let scratch_space = scratch.leave_it();
            return Err(err).context(FailureSnafu { scratch_space });
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

    #[snafu(display("The test failed the assertion: {text}"))]
    Assert {
        text: String,
        scratch_space: PathBuf,
    },

    #[snafu(display("The test returned an error"))]
    Failure {
        source: BoxError,
        scratch_space: PathBuf,
    },

    #[snafu(display("Could not shut down the registry"))]
    RegistryShutdown { source: BoxError },
}

impl TestError {
    fn scratch_space(&self) -> Option<&Path> {
        match self {
            Self::Assert { scratch_space, .. } | Self::Failure { scratch_space, .. } => {
                Some(scratch_space)
            }
            _ => None,
        }
    }
}

macro_rules! assert_downloaded_crates {
    ($scratch:expr, $expected:expr) => {
        let expected = $expected;
        let actual = $scratch.downloaded_crates().await?;
        assert!(
            expected == actual,
            "Should have downloaded {expected} crates, but there were {actual}",
        );
    };
}

const COMPILATION_FAILURE: &str = "const _: () = panic!();";
const CURRENT_TARGET: &str = env!("CURRENT_TARGET");

async fn name_length_1(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    parameterized_name(scratch, registry, "a").await
}

async fn name_length_2(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    parameterized_name(scratch, registry, "ab").await
}

async fn name_length_3(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    parameterized_name(scratch, registry, "abc").await
}

async fn name_length_4(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    parameterized_name(scratch, registry, "abcd").await
}

async fn parameterized_name(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
    name: &str,
) -> BoxResult {
    let library_crate = Crate::new(name, "0.1.0")
        .lib_rs("pub fn add(a: u8, b: u8) -> u8 { a + b }")
        .create_in(scratch)
        .await?;

    let registry_url = registry.registry_url().await;
    registry.publish_crate(&library_crate).await?;

    let reg = CreatedRegistry::new("mine", registry_url);

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.as_ref().in_registry(&reg))
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
) -> BoxResult {
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
    let reg = CreatedRegistry::new("mine", registry_url);

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(left_crate.in_registry(&reg))
        .add_dependency(right_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(7, left::add(1, right::mul(2, 3))); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn multiple_hierarchical_dependencies(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let two_away_crate = Crate::new("two", "0.1.0")
        .lib_rs("pub fn add(a: u8, b: u8) -> u8 { a + b }")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&two_away_crate).await?;

    let one_away_crate = Crate::new("one", "0.2.0")
        .add_registry(&reg)
        .add_dependency(two_away_crate.in_registry(&reg))
        .lib_rs("pub fn triple(a: u8) -> u8 { two::add(a, two::add(a, a)) }")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&one_away_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(one_away_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(9, one::triple(3)); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn cross_registry_dependencies(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let library_crate = Crate::new("the-library", "0.2.0")
        .add_dependency(("either", "1.0"))
        .lib_rs(
            "pub fn iter(c: bool) -> impl Iterator<Item = u8> {
                 if c {
                     either::Either::Left([1u8].into_iter())
                 } else {
                     either::Either::Right([2u8, 3].into_iter())
                 }
             }",
        )
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(2, the_library::iter(false).count()); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn crates_io_dependencies_unify(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let either_dep = ("either", "1.0");

    let library_crate = Crate::new("the-library", "0.2.0")
        .add_dependency(either_dep)
        .lib_rs("pub fn consume(_: either::Either<(), ()>) {}")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .add_dependency(either_dep)
        .main_rs("fn main() { the_library::consume(either::Either::Left(())); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn multiple_versions(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let library_v1 = Crate::new("the-library", "1.0.0")
        .lib_rs("pub const VERSION: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_v1).await?;

    let library_v2 = Crate::new("the-library", "2.0.0")
        .lib_rs("pub const VERSION: u8 = 2;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_v2).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_v1.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(1, the_library::VERSION); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_v2.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(2, the_library::VERSION); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;

    Ok(())
}

async fn conflicting_links(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);
    let links_key = "native-library";

    let conflict_one = Crate::new("conflict-one", "1.0.0")
        .links(links_key)
        .build_script("fn main() {}")
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&conflict_one).await?;

    let conflict_two = Crate::new("conflict-two", "1.0.0")
        .links(links_key)
        .build_script("fn main() {}")
        .lib_rs("pub const ID: u8 = 2;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&conflict_two).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(conflict_one.in_registry(&reg))
        .add_dependency(conflict_two.in_registry(&reg))
        .main_rs("fn main() {}")
        .create_in(scratch)
        .await?;

    usage_crate.cargo().run().command().expect_failure().await?;
    // If the registry doesn't put the links in the index, then Cargo
    // will download the crates and only find the conflict later.
    assert_downloaded_crates!(scratch, 0);

    Ok(())
}

async fn minimum_version(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let current_crate = Crate::new("from-the-future", "1.0.0")
        .rust_version("1.56")
        .lib_rs("pub const ID: u16 = 56;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&current_crate).await?;

    let future_crate = Crate::new("from-the-future", "1.1.0")
        .rust_version("1.1111")
        .lib_rs("pub const ID: u16 = 1111;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&future_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(current_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(56, from_the_future::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate
        .cargo()
        .use_nightly()
        .enable_msrv_resolver()
        .run()
        .command()
        .expect_success()
        .await?;

    Ok(())
}

async fn optional_dependency_unused(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let unused_dependency = Crate::new("unused", "1.0.0")
        .lib_rs(COMPILATION_FAILURE)
        .create_in(scratch)
        .await?;
    registry.publish_crate(&unused_dependency).await?;

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_dependency(unused_dependency.in_registry(&reg).optional())
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(1, the_library::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 1);

    Ok(())
}

async fn optional_dependency_used_implicit(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let used_dependency = Crate::new("used", "1.0.0")
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&used_dependency).await?;

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_dependency(used_dependency.as_ref().in_registry(&reg).optional())
        .lib_rs("pub use used::ID;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(
            library_crate
                .in_registry(&reg)
                .with_feature(&used_dependency.name),
        )
        .main_rs("fn main() { assert_eq!(1, the_library::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 2);

    Ok(())
}

async fn optional_dependency_used_implicit_renamed(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let rename = "another";

    let used_dependency = Crate::new("used", "1.0.0")
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&used_dependency).await?;

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_dependency(
            used_dependency
                .as_ref()
                .in_registry(&reg)
                .renamed_as(rename)
                .optional(),
        )
        .lib_rs("pub use another::ID;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg).with_feature(rename))
        .main_rs("fn main() { assert_eq!(1, the_library::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 2);

    Ok(())
}

async fn optional_dependency_used_explicit(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let used_dependency = Crate::new("used", "1.0.0")
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&used_dependency).await?;

    let feature_name = "my-feature";

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_dependency(used_dependency.as_ref().in_registry(&reg).optional())
        .add_feature(feature_name, ["dep:used"])
        .lib_rs("pub use used::ID;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg).with_feature(feature_name))
        .main_rs("fn main() { assert_eq!(1, the_library::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 2);

    Ok(())
}

async fn platform_specific_dependency_unused(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let not_this_platform = Crate::new("not-this-platform", "1.0.0")
        .lib_rs(COMPILATION_FAILURE)
        .create_in(scratch)
        .await?;
    registry.publish_crate(&not_this_platform).await?;

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_target_dependency("not-a-real-target", not_this_platform.in_registry(&reg))
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(1, the_library::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 1);

    Ok(())
}

async fn platform_specific_dependency_used(
    scratch: &ScratchSpace,
    registry: &mut impl Registry,
) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let this_platform = Crate::new("this-platform", "1.0.0")
        .lib_rs("pub const ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&this_platform).await?;

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_target_dependency(CURRENT_TARGET, this_platform.in_registry(&reg))
        .lib_rs("pub use this_platform::ID;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .main_rs("fn main() { assert_eq!(1, the_library::ID); }")
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 2);

    Ok(())
}

async fn build_only_dependency(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let only_for_build = Crate::new("only-for-build", "1.0.0")
        .lib_rs("pub const BUILD_ID: u8 = 1;")
        .create_in(scratch)
        .await?;
    registry.publish_crate(&only_for_build).await?;

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .add_build_dependency(only_for_build.in_registry(&reg))
        .build_script(r#"fn main() { println!("cargo::rustc-env=BUILD_ID_2={}", only_for_build::BUILD_ID); }"#)
        .lib_rs(r#"pub const ID: &str = env!("BUILD_ID_2");"#)
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .main_rs(r#"fn main() { assert_eq!("1", the_library::ID); }"#)
        .create_in(scratch)
        .await?;

    usage_crate.run().await?;
    assert_downloaded_crates!(scratch, 2);

    Ok(())
}

fn authorization_required_setup<B: RegistryBuilder>() -> B {
    B::default().enable_basic_auth("admin", "baseball123")
}

async fn authorization_required(scratch: &ScratchSpace, registry: &mut impl Registry) -> BoxResult {
    let registry_url = registry.registry_url().await;
    let reg = CreatedRegistry::new("mine", registry_url);

    let library_crate = Crate::new("the-library", "1.0.0")
        .add_registry(&reg)
        .lib_rs(r#"pub const ID: u8 = 1;"#)
        .create_in(scratch)
        .await?;
    registry.publish_crate(&library_crate).await?;

    let usage_crate = Crate::new("the-binary", "0.1.0")
        .add_registry(&reg)
        .add_dependency(library_crate.in_registry(&reg))
        .main_rs(r#"fn main() { assert_eq!(1, the_library::ID); }"#)
        .create_in(scratch)
        .await?;

    usage_crate
        .cargo()
        .basic_auth_credentials(&reg, "admin", "baseball123")
        .run()
        .command()
        .expect_success()
        .await?;

    Ok(())
}

pub struct ScratchSpace {
    #[allow(unused)]
    root: TempDir,
    cargo_home_path: PathBuf,
    crates_path: PathBuf,
    registry_path: PathBuf,
}

impl ScratchSpace {
    pub async fn new() -> Result<Self, ScratchSpaceError> {
        use scratch_space_error::*;

        let root = TempDir::new().context(RootSnafu)?;

        let cargo_home_path = root.path().join("cargo-home");
        fs::create_dir_all(&cargo_home_path)
            .await
            .context(CargoHomeCreateSnafu {
                path: &cargo_home_path,
            })?;

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
            cargo_home_path,
            crates_path,
            registry_path,
        })
    }

    pub fn root(&self) -> &Path {
        self.root.path()
    }

    pub fn cargo_home(&self) -> &Path {
        &self.cargo_home_path
    }

    pub fn crates(&self) -> &Path {
        &self.crates_path
    }

    pub fn registry(&self) -> &Path {
        &self.registry_path
    }

    async fn downloaded_crates(&self) -> Result<usize, DownloadedCratesError> {
        use downloaded_crates_error::*;

        let mut cache_path = self.cargo_home_path.join("registry");
        cache_path.push("cache");

        let mut d = match fs::read_dir(&cache_path).await {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).context(OpenCacheSnafu { path: cache_path }),
        };

        let mut reg = None;
        while let Some(entry) = d
            .next_entry()
            .await
            .context(EnumerateCacheSnafu { path: &cache_path })?
        {
            assert!(reg.is_none(), "Too many registries here");
            reg = Some(entry.file_name());
        }
        let Some(reg) = reg else { return Ok(0) };
        cache_path.push(reg);

        let mut d = match fs::read_dir(&cache_path).await {
            Ok(d) => d,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e).context(OpenRegistrySnafu { path: cache_path }),
        };

        let mut cnt = 0;
        while d
            .next_entry()
            .await
            .context(EnumerateRegistrySnafu { path: &cache_path })?
            .is_some()
        {
            cnt += 1;
        }

        Ok(cnt)
    }

    pub fn leave_it(self) -> PathBuf {
        self.root.into_path()
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum ScratchSpaceError {
    #[snafu(display("Could not create the scratch space root"))]
    Root { source: std::io::Error },

    #[snafu(display("Could not create the Cargo home directory at {}", path.display()))]
    CargoHomeCreate {
        source: std::io::Error,
        path: PathBuf,
    },

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

#[derive(Debug, Snafu)]
#[snafu(module)]
enum DownloadedCratesError {
    #[snafu(display("Could not open the cache directory {}", path.display()))]
    OpenCache {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not enumerate the cache directory {}", path.display()))]
    EnumerateCache {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not open the registry directory {}", path.display()))]
    OpenRegistry {
        source: std::io::Error,
        path: PathBuf,
    },

    #[snafu(display("Could not enumerate the registry directory {}", path.display()))]
    EnumerateRegistry {
        source: std::io::Error,
        path: PathBuf,
    },
}

pub trait RegistryBuilder: Sized + Default {
    type Registry: Registry;
    type Error: snafu::Error + Send + Sync + 'static;

    fn enable_basic_auth(self, username: &str, password: &str) -> Self;

    fn start(
        self,
        path: impl Into<PathBuf>,
    ) -> impl Future<Output = Result<Self::Registry, Self::Error>>;
}

pub trait Registry: Sized {
    type Builder: RegistryBuilder<Registry = Self>;
    type Error: snafu::Error + Send + Sync + 'static;

    fn registry_url(&self) -> impl Future<Output = String> + Send;

    fn publish_crate(
        &mut self,
        crate_: &CreatedCrate,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    fn shutdown(self) -> impl Future<Output = Result<(), Self::Error>>;
}

mod definition {
    pub trait RegistryDefinition {
        fn name(&self) -> &str;
        fn url(&self) -> &str;
    }

    impl<R: RegistryDefinition> RegistryDefinition for &R {
        fn name(&self) -> &str {
            R::name(self)
        }

        fn url(&self) -> &str {
            R::url(self)
        }
    }

    pub trait DependencyDefinition: Sized {
        fn finish(&self) -> Dependency2<'_>;

        fn as_ref(&self) -> &Self {
            self
        }

        fn in_registry<R>(self, reg: R) -> InRegistry<Self, R> {
            InRegistry(self, reg)
        }

        fn renamed_as(self, name: impl Into<String>) -> RenamedAs<Self> {
            RenamedAs(self, name.into())
        }

        fn optional(self) -> Optional<Self> {
            Optional(self)
        }

        fn with_feature(self, name: impl Into<String>) -> WithFeature<Self> {
            WithFeature(self, name.into())
        }
    }

    impl<D: DependencyDefinition> DependencyDefinition for &D {
        fn finish(&self) -> Dependency2<'_> {
            D::finish(self)
        }
    }

    impl DependencyDefinition for (&str, &str) {
        fn finish(&self) -> Dependency2<'_> {
            Dependency2::base(self.0, self.1)
        }
    }

    pub struct Dependency2<'a> {
        pub name: &'a str,
        pub version: &'a str,
        pub registry: Option<&'a str>,
        pub optional: bool,
        pub features: Option<Vec<&'a str>>,
        pub package: Option<&'a str>,
    }

    impl<'a> Dependency2<'a> {
        pub fn base(name: &'a str, version: &'a str) -> Self {
            Self {
                name,
                version,
                registry: None,
                optional: false,
                features: None,
                package: None,
            }
        }
    }

    pub struct InRegistry<D, R>(D, R);

    impl<D, R> DependencyDefinition for InRegistry<D, R>
    where
        D: DependencyDefinition,
        R: RegistryDefinition,
    {
        fn finish(&self) -> Dependency2<'_> {
            let mut dep = self.0.finish();
            dep.registry = Some(self.1.name());
            dep
        }
    }

    pub struct RenamedAs<D>(D, String);

    impl<D> DependencyDefinition for RenamedAs<D>
    where
        D: DependencyDefinition,
    {
        fn finish(&self) -> Dependency2<'_> {
            let mut dep = self.0.finish();
            let package = dep.name;
            dep.name = &self.1;
            dep.package.get_or_insert(package);
            dep
        }
    }

    pub struct Optional<D>(D);

    impl<D> DependencyDefinition for Optional<D>
    where
        D: DependencyDefinition,
    {
        fn finish(&self) -> Dependency2<'_> {
            let mut dep = self.0.finish();
            dep.optional = true;
            dep
        }
    }

    pub struct WithFeature<D>(D, String);

    impl<D> DependencyDefinition for WithFeature<D>
    where
        D: DependencyDefinition,
    {
        fn finish(&self) -> Dependency2<'_> {
            let mut dep = self.0.finish();
            dep.features
                .get_or_insert_with(Default::default)
                .push(&self.1);
            dep
        }
    }
}

#[derive(Debug)]
struct CreatedRegistry {
    name: String,
    url: String,
}

impl CreatedRegistry {
    fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
        }
    }
}

impl RegistryDefinition for CreatedRegistry {
    fn name(&self) -> &str {
        &self.name
    }

    fn url(&self) -> &str {
        &self.url
    }
}

#[derive(Debug)]
pub struct Crate {
    cargo_toml: cargo_toml::Root,
    src: BTreeMap<PathBuf, String>,
    dotcargo_config: Option<dotcargo_config::Root>,
    build_script: Option<String>,
}

impl Crate {
    pub fn new(name: impl Into<String>, version: impl Into<String>) -> Crate {
        Self {
            cargo_toml: cargo_toml::Root {
                package: cargo_toml::Package {
                    name: name.into(),
                    version: version.into(),
                    edition: "2021".into(),
                    links: Default::default(),
                    rust_version: Default::default(),
                },
                dependencies: Default::default(),
                build_dependencies: Default::default(),
                features: Default::default(),
                target: Default::default(),
            },
            src: Default::default(),
            dotcargo_config: Default::default(),
            build_script: Default::default(),
        }
    }

    pub fn lib_rs(mut self, contents: impl Into<String>) -> Self {
        self.src.insert("lib.rs".into(), contents.into());
        self
    }

    pub fn main_rs(mut self, contents: impl Into<String>) -> Self {
        self.src.insert("main.rs".into(), contents.into());
        self
    }

    pub fn add_registry(mut self, registry: impl RegistryDefinition) -> Self {
        let dotcargo_config = self.dotcargo_config.get_or_insert_with(Default::default);
        dotcargo_config.registries.insert(
            registry.name().to_owned(),
            dotcargo_config::Registry {
                index: registry.url().to_owned(),
                credential_provider: vec!["cargo:token".into()],
            },
        );
        self
    }

    pub fn add_dependency(mut self, dependency: impl DependencyDefinition) -> Self {
        Self::add_dependency_common(&mut self.cargo_toml.dependencies, dependency);
        self
    }

    pub fn add_build_dependency(mut self, dependency: impl DependencyDefinition) -> Self {
        Self::add_dependency_common(&mut self.cargo_toml.build_dependencies, dependency);
        self
    }

    pub fn add_target_dependency(
        mut self,
        target: impl Into<String>,
        dependency: impl DependencyDefinition,
    ) -> Self {
        let target = self.cargo_toml.target.entry(target.into()).or_default();
        Self::add_dependency_common(&mut target.dependencies, dependency);
        self
    }

    fn add_dependency_common(
        deps: &mut cargo_toml::Dependencies,
        dependency: impl DependencyDefinition,
    ) {
        let Dependency2 {
            name,
            version,
            registry,
            optional,
            features,
            package,
        } = dependency.finish();

        deps.insert(
            name.to_owned(),
            cargo_toml::Dependency {
                version: version.to_owned(),
                registry: registry.map(ToOwned::to_owned),
                optional,
                features: features.map(|f| f.into_iter().map(ToOwned::to_owned).collect()),
                package: package.map(ToOwned::to_owned),
            },
        );
    }

    pub fn add_feature(
        mut self,
        name: impl Into<String>,
        members: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let name = name.into();
        let members = members.into_iter().map(Into::into).collect();
        self.cargo_toml.features.insert(name, members);
        self
    }

    pub fn links(mut self, links_key: impl Into<String>) -> Self {
        self.cargo_toml.package.links = Some(links_key.into());
        self
    }

    pub fn build_script(mut self, contents: impl Into<String>) -> Self {
        self.build_script = Some(contents.into());
        self
    }

    pub fn rust_version(mut self, arg: impl Into<String>) -> Self {
        self.cargo_toml.package.rust_version = Some(arg.into());
        self
    }

    pub async fn create_in(self, scratch: &ScratchSpace) -> Result<CreatedCrate, CreateCrateError> {
        use create_crate_error::*;

        let mut crate_path = scratch.crates_path.join(&self.cargo_toml.package.name);
        crate_path.push(&self.cargo_toml.package.version);
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

        if let Some(build_script) = &self.build_script {
            let build_script_path = crate_path.join("build.rs");
            fs::write(&build_script_path, build_script)
                .await
                .context(BuildScriptWriteSnafu {
                    path: build_script_path,
                })?;
        }

        let Self { cargo_toml, .. } = self;
        let cargo_toml::Root { package, .. } = cargo_toml;
        let cargo_toml::Package { name, version, .. } = package;

        Ok(CreatedCrate {
            cargo_home: scratch.cargo_home_path.clone(),
            directory: crate_path,
            name,
            version,
        })
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum CreateCrateError {
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

    #[snafu(display("Could not write the build script {}", path.display()))]
    BuildScriptWrite {
        source: std::io::Error,
        path: PathBuf,
    },
}

#[derive(Debug)]
pub struct CreatedCrate {
    cargo_home: PathBuf,
    directory: PathBuf,
    name: String,
    version: String,
}

impl CreatedCrate {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub async fn package(&self) -> Result<PathBuf, PackageError> {
        use package_error::*;

        self.cargo()
            .package()
            .append_arg("--no-verify")
            .command()
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

    async fn run(&self) -> Result<CommandOutput, CommandError> {
        self.cargo().run().command().expect_success().await
    }

    fn cargo(&self) -> CargoCommandBuilder<'_> {
        CargoCommandBuilder {
            crate_: self,
            toolchain: Default::default(),
            msrv_resolver: Default::default(),
            credentials: Default::default(),
            command: Default::default(),
            args: Default::default(),
        }
    }

    fn package_path(&self) -> PathBuf {
        let Self {
            directory,
            name,
            version,
            ..
        } = self;

        let mut package_path = directory.join("target");
        package_path.push("package");
        package_path.push(format!("{name}-{version}.crate"));
        package_path
    }
}

impl DependencyDefinition for CreatedCrate {
    fn finish(&self) -> Dependency2<'_> {
        Dependency2::base(&self.name, &self.version)
    }
}

struct CargoCommandBuilder<'a> {
    crate_: &'a CreatedCrate,
    toolchain: Option<String>,
    msrv_resolver: bool,
    credentials: Option<(String, String)>,
    command: Option<String>,
    args: Option<Vec<String>>,
}

impl CargoCommandBuilder<'_> {
    fn use_nightly(mut self) -> Self {
        self.toolchain = Some("nightly".into());
        self
    }

    fn enable_msrv_resolver(mut self) -> Self {
        self.msrv_resolver = true;
        self
    }

    fn basic_auth_credentials(
        mut self,
        registry: &CreatedRegistry,
        username: &str,
        password: &str,
    ) -> Self {
        let name = registry.name().to_ascii_uppercase();

        let creds = format!("{username}:{password}");
        let mut token = String::from("Basic ");
        base64::engine::general_purpose::STANDARD.encode_string(creds, &mut token);

        self.credentials = Some((name, token));
        self
    }

    fn package(mut self) -> Self {
        self.command = Some("package".into());
        self
    }

    fn run(mut self) -> Self {
        self.command = Some("run".into());
        self
    }

    fn append_arg(mut self, arg: impl Into<String>) -> Self {
        let args = self.args.get_or_insert_with(Default::default);
        args.push(arg.into());
        self
    }

    fn command(self) -> Command {
        let mut cmd = Command::new("cargo");

        let toolchain = self.toolchain.as_deref().unwrap_or("stable");

        cmd.current_dir(&self.crate_.directory)
            .env("CARGO_HOME", &self.crate_.cargo_home)
            .env("RUSTUP_TOOLCHAIN", toolchain)
            .kill_on_drop(true);

        if let Some(command) = self.command {
            cmd.arg(command);
        }

        if let Some(args) = self.args {
            cmd.args(args);
        }

        if self.msrv_resolver {
            cmd.env(
                "CARGO_RESOLVER_SOMETHING_LIKE_PRECEDENCE",
                "something-like-rust-version",
            );
            cmd.arg("-Zmsrv-policy");
        }

        if let Some((name, credentials)) = self.credentials {
            let name = format!("CARGO_REGISTRIES_{name}_TOKEN");
            cmd.env(name, credentials);
        }

        cmd
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

mod dotcargo_config {
    use serde::Serialize;
    use std::collections::BTreeMap;

    #[derive(Debug, Default, Serialize)]
    pub struct Root {
        pub registries: BTreeMap<String, Registry>,
    }

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Registry {
        pub index: String,
        pub credential_provider: Vec<String>,
    }
}

mod cargo_toml {
    use serde::Serialize;
    use std::collections::BTreeMap;

    pub type Dependencies = BTreeMap<String, Dependency>;

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Root {
        pub package: Package,
        pub dependencies: Dependencies,
        pub build_dependencies: Dependencies,
        pub features: BTreeMap<String, Vec<String>>,
        pub target: BTreeMap<String, Target>,
    }

    #[derive(Debug, Serialize)]
    #[serde(rename_all = "kebab-case")]
    pub struct Package {
        pub name: String,
        pub version: String,
        pub edition: String,
        pub links: Option<String>,
        pub rust_version: Option<String>,
    }

    #[derive(Debug, Serialize)]
    pub struct Dependency {
        pub version: String,
        pub registry: Option<String>,
        pub optional: bool,
        pub features: Option<Vec<String>>,
        pub package: Option<String>,
    }

    #[derive(Debug, Default, Serialize)]
    pub struct Target {
        pub dependencies: Dependencies,
    }
}

#[derive(Debug)]
pub struct CommandOutput {
    stdout: String,
    stderr: String,
}

impl From<&process::Output> for CommandOutput {
    fn from(output: &process::Output) -> Self {
        let stdout = String::from_utf8_lossy(&output.stdout).into();
        let stderr = String::from_utf8_lossy(&output.stderr).into();

        Self { stdout, stderr }
    }
}

#[derive(Debug, Snafu)]
#[snafu(module)]
pub enum CommandError {
    #[snafu(transparent)]
    Output { source: std::io::Error },

    #[snafu(display("The command succeeded.\nstdout:\n{stdout}\nstderr:\n{stderr}"))]
    SuccessExit { stdout: String, stderr: String },

    #[snafu(display("The command failed.\nstdout:\n{stdout}\nstderr:\n{stderr}"))]
    ErrorExit { stdout: String, stderr: String },
}

#[allow(async_fn_in_trait)]
pub trait CommandExt {
    async fn expect_success(&mut self) -> Result<CommandOutput, CommandError>;
    async fn expect_failure(&mut self) -> Result<CommandOutput, CommandError>;
}

impl CommandExt for Command {
    async fn expect_success(&mut self) -> Result<CommandOutput, CommandError> {
        use command_error::*;

        let out = self.output().await?;
        let output = CommandOutput::from(&out);

        ensure!(
            out.status.success(),
            ErrorExitSnafu {
                stdout: output.stdout,
                stderr: output.stderr,
            }
        );

        Ok(output)
    }

    async fn expect_failure(&mut self) -> Result<CommandOutput, CommandError> {
        use command_error::*;

        let out = self.output().await?;
        let output = CommandOutput::from(&out);

        ensure!(
            !out.status.success(),
            SuccessExitSnafu {
                stdout: output.stdout,
                stderr: output.stderr,
            }
        );

        Ok(output)
    }
}
