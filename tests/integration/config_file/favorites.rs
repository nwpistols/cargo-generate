use predicates::prelude::*;

use crate::helpers::project::binary;
use crate::helpers::{create_template, project::Project, project_builder::tmp_dir};

use assert_cmd::prelude::*;
use indoc::indoc;
use std::path::PathBuf;

fn create_favorite_config(name: &str, template_path: &Project) -> (Project, PathBuf) {
    let project = tmp_dir()
        .file(
            "cargo-generate",
            &format!(
                indoc! {r#"
                    [favorites.{name}]
                    description = "Favorite for the {name} template"
                    git = "{git}"
                    branch = "{branch}"
                    "#},
                name = name,
                git = template_path.path().display().to_string().escape_default(),
                branch = "main"
            ),
        )
        .build();
    let path = project.path().join("cargo-generate");
    (project, path)
}

#[test]
fn favorite_with_git_becomes_subfolder() {
    let favorite_template = create_template("favorite-template");
    let git_template = create_template("git-template");
    let (_config, config_path) = create_favorite_config("test", &favorite_template);
    let working_dir = tmp_dir().build();

    binary()
        .arg("generate")
        .arg("--config")
        .arg(config_path)
        .arg("--name")
        .arg("foobar-project")
        .arg("--git")
        .arg(git_template.path())
        .arg("test")
        .current_dir(&working_dir.path())
        .assert()
        .failure();
}

#[test]
fn favorite_subfolder_must_be_valid() {
    let template = tmp_dir()
        .file("Cargo.toml", "")
        .file(
            "inner/Cargo.toml",
            indoc! {r#"
                [package]
                name = "{{project-name}}"
                description = "A wonderful project"
                version = "0.1.0"
            "#},
        )
        .init_git()
        .build();
    let working_dir = tmp_dir().build();

    binary()
        .arg("generate")
        .arg("-n")
        .arg("outer")
        .arg(template.path())
        .arg("Cargo.toml")
        .current_dir(&working_dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("must be a valid folder").from_utf8());

    binary()
        .arg("generate")
        .arg("-n")
        .arg("outer")
        .arg(template.path())
        .arg("non-existant")
        .current_dir(&working_dir.path())
        .assert()
        .failure(); // Error text is OS specific

    binary()
        .arg("generate")
        .arg("-n")
        .arg("outer")
        .arg(template.path())
        .arg(working_dir.path().parent().unwrap())
        .current_dir(&working_dir.path())
        .assert()
        .failure()
        .stderr(predicates::str::contains("Invalid subfolder.").from_utf8());
}

#[test]
fn favorite_with_subfolder() -> anyhow::Result<()> {
    let template = tmp_dir()
        .file("Cargo.toml", "")
        .file(
            "inner/Cargo.toml",
            indoc! {r#"
                [package]
                name = "{{project-name}}"
                description = "A wonderful project"
                version = "0.1.0"
            "#},
        )
        .init_git()
        .build();

    let working_dir = tmp_dir().build();
    binary()
        .arg("generate")
        .arg("-n")
        .arg("outer")
        .arg(template.path())
        .arg("inner")
        .current_dir(&working_dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Done!").from_utf8());

    assert!(working_dir.read("outer/Cargo.toml").contains("outer"));
    Ok(())
}

#[test]
fn it_can_use_favorites() {
    let favorite_template = create_template("favorite-template");
    let (_config, config_path) = create_favorite_config("test", &favorite_template);
    let working_dir = tmp_dir().build();

    binary()
        .arg("generate")
        .arg("--config")
        .arg(config_path)
        .arg("--name")
        .arg("favorite-project")
        .arg("test")
        .current_dir(&working_dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Done!").from_utf8());

    assert!(working_dir
        .read("favorite-project/Cargo.toml")
        .contains(r#"description = "favorite-template""#));
}

#[test]
fn favorites_default_to_git_if_not_defined() {
    let favorite_template = create_template("favorite-template");
    let (_config, config_path) = create_favorite_config("test", &favorite_template);
    let working_dir = tmp_dir().build();

    binary()
        .arg("generate")
        .arg("--config")
        .arg(config_path)
        .arg("--name")
        .arg("favorite-project")
        .arg("dummy")
        .current_dir(&working_dir.path())
        .assert()
        .failure()
        .stderr(
            predicates::str::contains(r#"Please check if the Git user / repository exists"#)
                .from_utf8(),
        );
}

#[test]
fn favorites_can_use_default_values() {
    let favorite_template_dir = tmp_dir()
        .file(
            "Cargo.toml",
            indoc! {r#"
            [package]
            name = "{{project-name}}"
            description = "{{my_value}}"
            version = "0.1.0"
        "#},
        )
        .init_git()
        .build();

    let config_dir = tmp_dir()
        .file(
            "cargo-generate.toml",
            &format!(
                indoc! {r#"
                [favorites.favorite]
                git = "{git}"

                [favorites.favorite.values]
                my_value = "Hello World"
                "#},
                git = favorite_template_dir
                    .path()
                    .display()
                    .to_string()
                    .escape_default(),
            ),
        )
        .build();

    let working_dir = tmp_dir().build();

    binary()
        .arg("generate")
        .arg("--config")
        .arg(config_dir.path().join("cargo-generate.toml"))
        .arg("--name")
        .arg("my-project")
        .arg("favorite")
        .current_dir(&working_dir.path())
        .assert()
        .success()
        .stdout(predicates::str::contains("Done!").from_utf8());

    assert!(working_dir
        .read("my-project/Cargo.toml")
        .contains(r#"description = "Hello World""#));
}

#[test]
fn favorites_default_value_can_be_overridden_by_environment() {
    let values_dir = tmp_dir()
        .file(
            "values_file.toml",
            indoc! {r#"
            [values]
            my_value = "Overridden value"
        "#},
        )
        .build();

    let favorite_template_dir = tmp_dir()
        .file(
            "Cargo.toml",
            indoc! {r#"
            [package]
            name = "{{project-name}}"
            description = "{{my_value}}"
            version = "0.1.0"
        "#},
        )
        .init_git()
        .build();

    let config_dir = tmp_dir()
        .file(
            "cargo-generate.toml",
            &format!(
                indoc! {r#"
                [favorites.favorite]
                git = "{git}"

                [favorites.favorite.values]
                my_value = "Hello World"
                "#},
                git = favorite_template_dir
                    .path()
                    .display()
                    .to_string()
                    .escape_default(),
            ),
        )
        .build();

    let working_dir = tmp_dir().build();

    binary()
        .arg("generate")
        .arg("--config")
        .arg(config_dir.path().join("cargo-generate.toml"))
        .arg("--name")
        .arg("my-project")
        .arg("favorite")
        .current_dir(&working_dir.path())
        .env(
            "CARGO_GENERATE_TEMPLATE_VALUES_FILE",
            values_dir.path().join("values_file.toml"),
        )
        .assert()
        .success()
        .stdout(predicates::str::contains("Done!").from_utf8());

    assert!(working_dir
        .read("my-project/Cargo.toml")
        .contains(r#"description = "Overridden value""#));
}
