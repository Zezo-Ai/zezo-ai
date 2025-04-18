use anyhow::Context as _;
use gpui::{App, Context, Entity, Window};
use language::Language;
use url::Url;
use workspace::{OpenOptions, OpenVisible};

use crate::lsp_ext::find_specific_language_server_in_selection;

use crate::{element::register_action, Editor, SwitchSourceHeader};

use project::lsp_store::clangd_ext::CLANGD_SERVER_NAME;

fn is_c_language(language: &Language) -> bool {
    return language.name() == "C++".into() || language.name() == "C".into();
}

pub fn switch_source_header(
    editor: &mut Editor,
    _: &SwitchSourceHeader,
    window: &mut Window,
    cx: &mut Context<Editor>,
) {
    let Some(project) = &editor.project else {
        return;
    };
    let Some(workspace) = editor.workspace() else {
        return;
    };

    let Some((_, _, server_to_query, buffer)) =
        find_specific_language_server_in_selection(editor, cx, is_c_language, CLANGD_SERVER_NAME)
    else {
        return;
    };

    let project = project.clone();
    let buffer_snapshot = buffer.read(cx).snapshot();
    let source_file = buffer_snapshot
        .file()
        .unwrap()
        .file_name(cx)
        .to_str()
        .unwrap()
        .to_owned();

    let switch_source_header_task = project.update(cx, |project, cx| {
        project.request_lsp(
            buffer,
            project::LanguageServerToQuery::Other(server_to_query),
            project::lsp_store::lsp_ext_command::SwitchSourceHeader,
            cx,
        )
    });
    cx.spawn_in(window, async move |_editor, cx| {
        let switch_source_header = switch_source_header_task
            .await
            .with_context(|| format!("Switch source/header LSP request for path \"{source_file}\" failed"))?;
        if switch_source_header.0.is_empty() {
            log::info!("Clangd returned an empty string when requesting to switch source/header from \"{source_file}\"" );
            return Ok(());
        }

        let goto = Url::parse(&switch_source_header.0).with_context(|| {
            format!(
                "Parsing URL \"{}\" returned from switch source/header failed",
                switch_source_header.0
            )
        })?;

        let path = goto.to_file_path().map_err(|()| {
            anyhow::anyhow!("URL conversion to file path failed for \"{goto}\"")
        })?;

        workspace
            .update_in(cx, |workspace, window, cx| {
                workspace.open_abs_path(path, OpenOptions { visible: Some(OpenVisible::None), ..Default::default() }, window, cx)
            })
            .with_context(|| {
                format!(
                    "Switch source/header could not open \"{goto}\" in workspace"
                )
            })?
            .await
            .map(|_| ())
    })
    .detach_and_log_err(cx);
}

pub fn apply_related_actions(editor: &Entity<Editor>, window: &mut Window, cx: &mut App) {
    if editor.update(cx, |e, cx| {
        find_specific_language_server_in_selection(e, cx, is_c_language, CLANGD_SERVER_NAME)
            .is_some()
    }) {
        register_action(editor, window, switch_source_header);
    }
}
