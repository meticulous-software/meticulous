use crate::{pattern, BuildDir, GoPackage, GoTestCollector, ProjectDir};
use anyhow::Result;
use maelstrom_test_runner::{ui::UiSender, CollectTests as _, TestPackage as _};
use maelstrom_util::{process::ExitCode, root::Root};

/// Returns `true` if the given `GoPackage` matches the given pattern
fn filter_package(package: &GoPackage, p: &pattern::Pattern) -> bool {
    let c = pattern::Context {
        package: package.name().into(),
        case: None,
    };
    pattern::interpret_pattern(p, &c).unwrap_or(true)
}

pub fn list_packages(
    ui: UiSender,
    project_dir: &Root<ProjectDir>,
    build_dir: &Root<BuildDir>,
    include: &[String],
    exclude: &[String],
) -> Result<ExitCode> {
    ui.update_enqueue_status("listing packages...");

    let collector = GoTestCollector::new(project_dir, build_dir);
    let packages = collector.get_packages(&ui)?;
    let filter = pattern::compile_filter(include, exclude)?;
    for package in packages {
        if filter_package(&package, &filter) {
            ui.list(package.name().into());
        }
    }
    Ok(ExitCode::SUCCESS)
}
