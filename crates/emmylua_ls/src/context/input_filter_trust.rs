//! The server runs the input filters of a configuration (`workspace.inputFilters`)
//! only once the user allows them. A workspace's configuration comes with the
//! workspace: without asking, a cloned repository would run its programs as soon
//! as its folder is opened. The user is asked once for each set of filters, before
//! the workspace is loaded with them, and the sets allowed are kept in the user's
//! data directory, outside any workspace.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use emmylua_code_analysis::{Emmyrc, EmmyrcInputFilter};
use lsp_types::{MessageActionItem, MessageType, ShowMessageRequestParams};
use tokio_util::sync::CancellationToken;

use super::ServerContextSnapshot;

#[derive(Debug, Default)]
pub struct InputFilterTrust {
    /// The sets of filters the user allowed, read from the data directory on first use.
    allowed: Option<Vec<Vec<EmmyrcInputFilter>>>,
    /// The sets of filters the user was asked about by this server.
    asked: Vec<Vec<EmmyrcInputFilter>>,
}

/// The configuration to load the workspace with: `emmyrc` if it has no input
/// filters or the user allows them, otherwise `emmyrc` without them. A set of
/// filters is asked about once, and the load waits for the answer, so that the
/// workspace is loaded once, with or without them.
pub async fn check_input_filters(
    context: &ServerContextSnapshot,
    emmyrc: Arc<Emmyrc>,
) -> Arc<Emmyrc> {
    let filters = emmyrc.workspace.input_filters.clone();
    if filters.is_empty() {
        return emmyrc;
    }
    let first_time = {
        let mut workspace_manager = context.workspace_manager().lock().await;
        let trust = &mut workspace_manager.input_filter_trust;
        if trust.allowed().contains(&filters) {
            return emmyrc;
        }
        let first_time = !trust.asked.contains(&filters);
        if first_time {
            trust.asked.push(filters.clone());
        }
        first_time
    };
    // Not under the lock: the user may take their time.
    if first_time && ask(context, &filters).await {
        let mut workspace_manager = context.workspace_manager().lock().await;
        workspace_manager.input_filter_trust.allow(filters);
        return emmyrc;
    }
    let mut without_filters = emmyrc.as_ref().clone();
    without_filters.workspace.input_filters.clear();
    Arc::new(without_filters)
}

impl InputFilterTrust {
    fn allowed(&mut self) -> &mut Vec<Vec<EmmyrcInputFilter>> {
        self.allowed.get_or_insert_with(|| {
            let Some(path) = allowed_file() else {
                return Vec::new();
            };
            let Ok(text) = std::fs::read_to_string(&path) else {
                return Vec::new();
            };
            serde_json::from_str(&text).unwrap_or_else(|err| {
                log::error!("Cannot read {:?}: {}", path, err);
                Vec::new()
            })
        })
    }

    fn allow(&mut self, filters: Vec<EmmyrcInputFilter>) {
        let allowed = self.allowed();
        if allowed.contains(&filters) {
            return;
        }
        allowed.push(filters);

        let Some(path) = allowed_file() else {
            return;
        };
        let saved = serde_json::to_string_pretty(allowed).map_err(|err| err.to_string());
        let saved = saved.and_then(|text| {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|err| err.to_string())?;
            }
            std::fs::write(&path, text).map_err(|err| err.to_string())
        });
        if let Err(err) = saved {
            log::error!("Cannot write {:?}: {}", path, err);
        }
    }
}

fn allowed_file() -> Option<PathBuf> {
    dirs::data_local_dir().map(|dir| dir.join("emmylua_ls").join("allowed_input_filters.json"))
}

/// Whether the user allows the programs of `filters` to run.
async fn ask(context: &ServerContextSnapshot, filters: &[EmmyrcInputFilter]) -> bool {
    let allow = t!("Allow").to_string();
    let commands = filters
        .iter()
        .map(|filter| {
            let mut command = vec![filter.program.as_str()];
            command.extend(filter.args.iter().map(String::as_str));
            format!("{}: {}", filter.glob, command.join(" "))
        })
        .collect::<Vec<_>>();
    let params = ShowMessageRequestParams {
        typ: MessageType::WARNING,
        message: format!(
            "{}\n{}",
            t!(
                "The configuration runs these programs to read workspace files. A malicious program can harm your computer."
            ),
            commands.join("\n")
        ),
        actions: Some(vec![
            MessageActionItem {
                title: allow.clone(),
                properties: HashMap::new(),
            },
            MessageActionItem {
                title: t!("Don't allow").to_string(),
                properties: HashMap::new(),
            },
        ]),
    };
    let answer = context
        .client()
        .show_message_request(params, CancellationToken::new())
        .await;
    answer.is_some_and(|item| item.title == allow)
}
