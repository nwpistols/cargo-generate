#![doc = include_str!("../README.md")]
#![warn(
    //clippy::cargo_common_metadata,
    clippy::branches_sharing_code,
    clippy::cast_lossless,
    clippy::cognitive_complexity,
    clippy::get_unwrap,
    clippy::if_then_some_else_none,
    clippy::inefficient_to_string,
    clippy::match_bool,
    clippy::missing_const_for_fn,
    clippy::missing_panics_doc,
    clippy::option_if_let_else,
    clippy::redundant_closure,
    clippy::redundant_else,
    clippy::redundant_pub_crate,
    clippy::ref_binding_to_reference,
    clippy::ref_option_ref,
    clippy::same_functions_in_if_condition,
    clippy::unneeded_field_pattern,
    clippy::unnested_or_patterns,
    clippy::use_self,
)]

mod app_config;
mod args;
mod config;
mod emoji;
mod favorites;
mod filenames;
mod git;
mod hooks;
mod ignore_me;
mod include_exclude;
mod interactive;
mod log;
mod progressbar;
mod project_variables;
mod template;
mod template_filters;
mod template_variables;
mod user_parsed_input;

pub use args::*;

use anyhow::{anyhow, bail, Context, Result};
use config::{locate_template_configs, Config, CONFIG_FILE_NAME};
use console::style;
use favorites::list_favorites;
use git::DEFAULT_BRANCH;
use hooks::{execute_post_hooks, execute_pre_hooks};
use ignore_me::remove_dir_files;
use interactive::prompt_for_variable;
use liquid::ValueView;
use project_variables::{StringEntry, TemplateSlots, VarInfo};
use std::ffi::OsString;
use std::{
    borrow::Borrow,
    cell::RefCell,
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    rc::Rc,
};
use user_parsed_input::{TemplateLocation, UserParsedInput};

use tempfile::TempDir;

use crate::template_variables::load_env_and_args_template_values;
use crate::{
    app_config::{app_config_path, AppConfig},
    project_variables::ConversionError,
    template_variables::{CrateType, ProjectName},
};

/// # Panics
pub fn generate(mut args: GenerateArgs) -> Result<()> {
    let app_config: AppConfig = app_config_path(&args.config)?.as_path().try_into()?;

    if args.list_favorites {
        return list_favorites(&app_config, &args);
    }

    if args.ssh_identity.is_none()
        && app_config.defaults.is_some()
        && app_config.defaults.as_ref().unwrap().ssh_identity.is_some()
    {
        args.ssh_identity = app_config
            .defaults
            .as_ref()
            .unwrap()
            .ssh_identity
            .as_ref()
            .cloned();
    }

    let mut source_template = UserParsedInput::try_from_args_and_config(&app_config, &args);
    source_template
        .template_values_mut()
        .extend(load_env_and_args_template_values(&args)?);

    let (template_base_dir, template_folder, branch) = prepare_local_template(&source_template)?;

    let template_config = Config::from_path(
        &locate_template_file(CONFIG_FILE_NAME, &template_base_dir, &template_folder).ok(),
    )?
    .unwrap_or_default();

    check_cargo_generate_version(&template_config)?;

    let base_dir = env::current_dir()?;
    let project_name = resolve_project_name(&args)?;
    let project_dir = resolve_project_dir(&base_dir, &project_name, &args)?;

    println!(
        "{} {} {}",
        emoji::WRENCH,
        style(format!("Basedir: {}", base_dir.display())).bold(),
        style("...").bold()
    );

    println!(
        "{} {} {}",
        emoji::WRENCH,
        style("Generating template").bold(),
        style("...").bold()
    );

    expand_template(
        &project_dir,
        &project_name,
        &template_folder,
        source_template.template_values(),
        template_config,
        &args,
    )?;

    println!(
        "{} {} `{}`{}",
        emoji::WRENCH,
        style("Moving generated files into:").bold(),
        style(project_dir.display()).bold().yellow(),
        style("...").bold()
    );
    copy_dir_all(&template_folder, &project_dir)?;

    if !args.vcs.is_none() && (!args.init || args.force_git_init) {
        info!("{}", style("Initializing a fresh Git repository").bold());
        args.vcs
            .initialize(&project_dir, branch, args.force_git_init)?;
    }

    println!(
        "{} {} {} {}",
        emoji::SPARKLE,
        style("Done!").bold().green(),
        style("New project created").bold(),
        style(&project_dir.display()).underlined()
    );
    Ok(())
}

fn prepare_local_template(
    source_template: &UserParsedInput,
) -> Result<(TempDir, PathBuf, String), anyhow::Error> {
    let (temp_dir, branch) = get_source_template_into_temp(source_template.location())?;
    let template_folder = resolve_template_dir(&temp_dir, source_template.subfolder())?;

    Ok((temp_dir, template_folder, branch))
}

fn get_source_template_into_temp(
    template_location: &TemplateLocation,
) -> Result<(TempDir, String)> {
    let temp_dir: TempDir;
    let branch: String;
    match template_location {
        TemplateLocation::Git(git) => {
            let (temp_dir2, branch2) =
                git::clone_git_template_into_temp(git.url(), git.branch(), git.identity())?;
            temp_dir = temp_dir2;
            branch = branch2;
        }
        TemplateLocation::Path(path) => {
            temp_dir = copy_path_template_into_temp(path)?;
            branch = String::from(DEFAULT_BRANCH); // FIXME is here any reason to set branch when path is used?
        }
    };

    Ok((temp_dir, branch))
}

fn resolve_project_name(args: &GenerateArgs) -> Result<ProjectName> {
    match args.name {
        Some(ref n) => Ok(ProjectName::new(n)),
        None if !args.silent => Ok(ProjectName::new(interactive::name()?)),
        None => Err(anyhow!(
            "{} {} {}",
            emoji::ERROR,
            style("Project Name Error:").bold().red(),
            style("Option `--silent` provided, but project name was not set. Please use `--name`.")
                .bold()
                .red(),
        )),
    }
}

fn resolve_template_dir(template_base_dir: &TempDir, subfolder: Option<&str>) -> Result<PathBuf> {
    if let Some(subfolder) = subfolder {
        let template_base_dir = fs::canonicalize(template_base_dir.path())?;
        let template_dir =
            fs::canonicalize(template_base_dir.join(subfolder)).with_context(|| {
                format!(
                    "not able to find subfolder '{}' in source template",
                    subfolder
                )
            })?;

        // make sure subfolder is not `../../subfolder`
        if !template_dir.starts_with(&template_base_dir) {
            return Err(anyhow!(
                "{} {} {}",
                emoji::ERROR,
                style("Subfolder Error:").bold().red(),
                style("Invalid subfolder. Must be part of the template folder structure.")
                    .bold()
                    .red(),
            ));
        }

        if !template_dir.is_dir() {
            return Err(anyhow!(
                "{} {} {}",
                emoji::ERROR,
                style("Subfolder Error:").bold().red(),
                style("The specified subfolder must be a valid folder.")
                    .bold()
                    .red(),
            ));
        }

        Ok(auto_locate_template_dir(
            &template_dir,
            prompt_for_variable,
        )?)
    } else {
        auto_locate_template_dir(template_base_dir.path(), prompt_for_variable)
    }
}

fn auto_locate_template_dir(
    template_base_dir: &Path,
    prompt: impl Fn(&TemplateSlots) -> Result<String>,
) -> Result<PathBuf> {
    let config_paths = locate_template_configs(template_base_dir)?;
    match config_paths.len() {
        0 => Ok(template_base_dir.to_owned()),
        1 => Ok(template_base_dir.join(&config_paths[0])),
        _ => {
            let prompt_args = TemplateSlots {
                prompt: "Which template should be expanded?".into(),
                var_name: "Template".into(),
                var_info: VarInfo::String {
                    entry: Box::new(StringEntry {
                        default: Some(config_paths[0].clone()),
                        choices: Some(config_paths),
                        regex: None,
                    }),
                },
            };
            let path = prompt(&prompt_args)?;
            Ok(template_base_dir.join(&path))
        }
    }
}

fn copy_path_template_into_temp(src_path: &Path) -> Result<TempDir> {
    let path_clone_dir = tempfile::tempdir()?;
    copy_dir_all(src_path, path_clone_dir.path())?;
    git::remove_history(path_clone_dir.path())?;

    Ok(path_clone_dir)
}

pub(crate) fn copy_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
    fn check_dir_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
        if !dst.as_ref().exists() {
            return Ok(());
        }

        for src_entry in fs::read_dir(src)? {
            let src_entry = src_entry?;
            let filename = src_entry.file_name().to_string_lossy().to_string();
            let entry_type = src_entry.file_type()?;

            if entry_type.is_dir() {
                let dst_path = dst.as_ref().join(filename);
                check_dir_all(src_entry.path(), dst_path)?;
            } else if entry_type.is_file() {
                let filename = filename.strip_suffix(".liquid").unwrap_or(&filename);
                let dst_path = dst.as_ref().join(filename);
                if dst_path.exists() {
                    bail!(
                        "{} {} {}",
                        crate::emoji::WARN,
                        style("File already exists:").bold().red(),
                        style(dst_path.display()).bold().red(),
                    )
                }
            } else {
                bail!(
                    "{} {}",
                    crate::emoji::WARN,
                    style("Symbolic links not supported").bold().red(),
                )
            }
        }
        Ok(())
    }
    fn copy_all(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> Result<()> {
        fs::create_dir_all(&dst)?;
        let git_file_name: OsString = ".git".into();
        for src_entry in fs::read_dir(src)? {
            let src_entry = src_entry?;
            let filename = src_entry.file_name().to_string_lossy().to_string();
            let entry_type = src_entry.file_type()?;
            if entry_type.is_dir() {
                let dst_path = dst.as_ref().join(filename);
                if git_file_name == src_entry.file_name() {
                    continue;
                }
                copy_dir_all(src_entry.path(), dst_path)?;
            } else if entry_type.is_file() {
                let filename = filename.strip_suffix(".liquid").unwrap_or(&filename);
                let dst_path = dst.as_ref().join(filename);
                fs::copy(src_entry.path(), dst_path)?;
            }
        }
        Ok(())
    }

    check_dir_all(&src, &dst)?;
    copy_all(src, dst)
}

fn locate_template_file(
    name: &str,
    template_base_folder: impl AsRef<Path>,
    template_folder: impl AsRef<Path>,
) -> Result<PathBuf> {
    let template_base_folder = template_base_folder.as_ref();
    let mut search_folder = template_folder.as_ref().to_path_buf();
    loop {
        let file_path = search_folder.join(name.borrow());
        if file_path.exists() {
            return Ok(file_path);
        }
        if search_folder == template_base_folder {
            bail!("File not found within template");
        }
        search_folder = search_folder
            .parent()
            .ok_or_else(|| anyhow!("Reached root folder"))?
            .to_path_buf();
    }
}

/// Resolves the project dir.
///
/// if `args.init == true` it returns the path of `$CWD` and if let some `args.destination`,
/// it returns the given path.
fn resolve_project_dir(
    base_dir: &Path,
    name: &ProjectName,
    args: &GenerateArgs,
) -> Result<PathBuf> {
    if args.init {
        return Ok(base_dir.into());
    }

    let base_path = args
        .destination
        .as_ref()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| env::current_dir().unwrap_or_else(|_| ".".into()));

    let dir_name = args.force.then(|| name.raw()).unwrap_or_else(|| {
        rename_warning(name);
        name.kebab_case()
    });

    let project_dir = base_path.join(&dir_name);

    if project_dir.exists() {
        bail!(
            "{} {}",
            emoji::ERROR,
            style("Target directory already exists, aborting!")
                .bold()
                .red()
        );
    }

    Ok(project_dir)
}

fn expand_template(
    project_dir: &Path,
    name: &ProjectName,
    dir: &Path,
    template_values: &HashMap<String, toml::Value>,
    mut template_config: Config,
    args: &GenerateArgs,
) -> Result<()> {
    let crate_type: CrateType = args.into();
    let liquid_object = template::create_liquid_object(args, project_dir, name, &crate_type)?;
    let liquid_object =
        project_variables::fill_project_variables(liquid_object, &template_config, |slot| {
            let provided_value = template_values.get(&slot.var_name).and_then(|v| v.as_str());
            if provided_value.is_none() && args.silent {
                anyhow::bail!(ConversionError::MissingPlaceholderVariable {
                    var_name: slot.var_name.clone()
                })
            }
            interactive::variable(slot, provided_value)
        })?;
    let liquid_object = add_missing_provided_values(liquid_object, template_values)?;
    let (mut template_cfg, liquid_object) =
        merge_conditionals(&template_config, liquid_object, args)?;

    let all_hook_files = template_config.get_hook_files();

    let mut liquid_object = Rc::new(RefCell::new(liquid_object));

    execute_pre_hooks(
        dir,
        Rc::clone(&liquid_object),
        &mut template_config,
        args.allow_commands,
        args.silent,
    )?;
    ignore_me::remove_unneeded_files(dir, &template_cfg.ignore, args.verbose)?;
    let mut pbar = progressbar::new();

    // SAFETY: We gave a clone of the Rc to `execute_pre_hooks` which by now has already been dropped. Therefore, there
    // is no other pointer into this Rc which makes it safe to `get_mut`.
    let liquid_object_ref = Rc::get_mut(&mut liquid_object).unwrap().get_mut();

    template::walk_dir(
        dir,
        liquid_object_ref,
        &mut template_cfg,
        &all_hook_files,
        &mut pbar,
    )?;
    pbar.join().unwrap();

    execute_post_hooks(
        dir,
        Rc::clone(&liquid_object),
        &template_config,
        args.allow_commands,
        args.silent,
    )?;
    remove_dir_files(all_hook_files, false);

    Ok(())
}

pub(crate) fn add_missing_provided_values(
    mut liquid_object: liquid::Object,
    template_values: &HashMap<String, toml::Value>,
) -> Result<liquid::Object, anyhow::Error> {
    template_values.iter().try_for_each(|(k, v)| {
        if liquid_object.contains_key(k.as_str()) {
            return Ok(());
        }
        let value = match v {
            toml::Value::String(content) => liquid_core::Value::Scalar(content.clone().into()),
            toml::Value::Boolean(content) => liquid_core::Value::Scalar((*content).into()),
            _ => anyhow::bail!(format!(
                "{} {}",
                emoji::ERROR,
                style("Unsupported value type. Only Strings and Booleans are supported.")
                    .bold()
                    .red(),
            )),
        };
        liquid_object.insert(k.clone().into(), value);
        Ok(())
    })?;
    Ok(liquid_object)
}

fn merge_conditionals(
    template_config: &Config,
    liquid_object: liquid::Object,
    args: &GenerateArgs,
) -> Result<(config::TemplateConfig, liquid::Object), anyhow::Error> {
    let mut template_config = (*template_config).clone();
    let mut template_cfg = template_config.template.unwrap_or_default();
    let conditionals = template_config.conditional.take();
    if conditionals.is_none() {
        return Ok((template_cfg, liquid_object));
    }

    let mut conditionals = conditionals.unwrap();
    let mut engine = rhai::Engine::new();
    #[allow(deprecated)]
    engine.on_var({
        let liqobj = liquid_object.clone();
        move |name, _, _| match liqobj.get(name) {
            Some(value) => Ok(value.as_view().as_scalar().map(|scalar| {
                scalar.to_bool().map_or_else(
                    || {
                        let v = scalar.to_kstr();
                        v.as_str().into()
                    },
                    |v| v.into(),
                )
            })),
            None => Ok(None),
        }
    });

    for (_, conditional_template_cfg) in conditionals
        .iter_mut()
        .filter(|(key, _)| engine.eval_expression::<bool>(key).unwrap_or_default())
    {
        if let Some(mut extra_includes) = conditional_template_cfg.include.take() {
            let mut includes = template_cfg.include.unwrap_or_default();
            includes.append(&mut extra_includes);
            template_cfg.include = Some(includes);
        }
        if let Some(mut extra_excludes) = conditional_template_cfg.exclude.take() {
            let mut excludes = template_cfg.exclude.unwrap_or_default();
            excludes.append(&mut extra_excludes);
            template_cfg.exclude = Some(excludes);
        }
        if let Some(mut extra_ignores) = conditional_template_cfg.ignore.take() {
            let mut ignores = template_cfg.ignore.unwrap_or_default();
            ignores.append(&mut extra_ignores);
            template_cfg.ignore = Some(ignores);
        }
        if let Some(extra_placeholders) = conditional_template_cfg.placeholders.take() {
            match template_config.placeholders.as_mut() {
                Some(placeholders) => {
                    for (k, v) in extra_placeholders.0 {
                        placeholders.0.insert(k, v);
                    }
                }
                None => {
                    template_config.placeholders = Some(extra_placeholders);
                }
            }
        }
    }

    template_config.template = Some(template_cfg);
    let template =
        project_variables::fill_project_variables(liquid_object, &template_config, |slot| {
            if args.silent {
                anyhow::bail!(ConversionError::MissingPlaceholderVariable {
                    var_name: slot.var_name.clone()
                })
            }
            interactive::variable(slot, None)
        })?;
    template_cfg = template_config.template.unwrap_or_default();

    Ok((template_cfg, template))
}

fn rename_warning(name: &ProjectName) {
    if !name.is_crate_name() {
        warn!(
            "{} `{}` {} `{}`{}",
            style("Renaming project called").bold(),
            style(&name.user_input).bold().yellow(),
            style("to").bold(),
            style(&name.kebab_case()).bold().green(),
            style("...").bold()
        );
    }
}

fn check_cargo_generate_version(template_config: &Config) -> Result<(), anyhow::Error> {
    if let Config {
        template:
            Some(config::TemplateConfig {
                cargo_generate_version: Some(requirement),
                ..
            }),
        ..
    } = template_config
    {
        let version = semver::Version::parse(env!("CARGO_PKG_VERSION"))?;
        if !requirement.matches(&version) {
            bail!(
                "{} {} {} {} {}",
                emoji::ERROR,
                style("Required cargo-generate version not met. Required:")
                    .bold()
                    .red(),
                style(requirement).yellow(),
                style(" was:").bold().red(),
                style(version).yellow(),
            );
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{auto_locate_template_dir, project_variables::VarInfo};
    use anyhow::anyhow;
    use std::{
        fs,
        io::Write,
        path::{Path, PathBuf},
    };
    use tempfile::{tempdir, TempDir};

    #[test]
    fn auto_locate_template_returns_base_when_no_cargo_generate_is_found() -> anyhow::Result<()> {
        let tmp = tempdir().unwrap();
        create_file(&tmp, "dir1/Cargo.toml", "")?;
        create_file(&tmp, "dir2/dir2_1/Cargo.toml", "")?;
        create_file(&tmp, "dir3/Cargo.toml", "")?;

        let r = auto_locate_template_dir(tmp.path(), |_slots| Err(anyhow!("test")))?;
        assert_eq!(tmp.path(), r);
        Ok(())
    }

    #[test]
    fn auto_locate_template_returns_path_when_single_cargo_generate_is_found() -> anyhow::Result<()>
    {
        let tmp = tempdir().unwrap();
        create_file(&tmp, "dir1/Cargo.toml", "")?;
        create_file(&tmp, "dir2/dir2_1/Cargo.toml", "")?;
        create_file(&tmp, "dir2/dir2_2/cargo-generate.toml", "")?;
        create_file(&tmp, "dir3/Cargo.toml", "")?;

        let r = auto_locate_template_dir(tmp.path(), |_slots| Err(anyhow!("test")))?;
        assert_eq!(tmp.path().join("dir2/dir2_2"), r);
        Ok(())
    }

    #[test]
    fn auto_locate_template_prompts_when_multiple_cargo_generate_is_found() -> anyhow::Result<()> {
        let tmp = tempdir().unwrap();
        create_file(&tmp, "dir1/Cargo.toml", "")?;
        create_file(&tmp, "dir2/dir2_1/Cargo.toml", "")?;
        create_file(&tmp, "dir2/dir2_2/cargo-generate.toml", "")?;
        create_file(&tmp, "dir3/Cargo.toml", "")?;
        create_file(&tmp, "dir4/cargo-generate.toml", "")?;

        let r = auto_locate_template_dir(tmp.path(), |slots| match &slots.var_info {
            VarInfo::Bool { .. } => anyhow::bail!("Wrong prompt type"),
            VarInfo::String { entry } => {
                if let Some(mut choices) = entry.choices.clone() {
                    choices.sort();
                    let expected = vec![
                        Path::new("dir2").join("dir2_2").to_string(),
                        "dir4".to_string(),
                    ];
                    assert_eq!(expected, choices);
                    Ok("my_path".to_string())
                } else {
                    anyhow::bail!("Missing choices")
                }
            }
        });
        assert_eq!(tmp.path().join("my_path"), r?);

        Ok(())
    }

    pub trait PathString {
        fn to_string(&self) -> String;
    }

    impl PathString for PathBuf {
        fn to_string(&self) -> String {
            self.as_path().to_string()
        }
    }

    impl PathString for Path {
        fn to_string(&self) -> String {
            self.display().to_string()
        }
    }

    pub fn create_file(base_path: &TempDir, path: &str, contents: &str) -> anyhow::Result<()> {
        let path = base_path.path().join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        fs::File::create(&path)?.write_all(contents.as_ref())?;
        Ok(())
    }
}
