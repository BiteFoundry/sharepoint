//! SharePoint Lists CLI subcommand handler.

use anyhow::Result;
use serde_json::json;

use crate::{config::AppConfig, safe_text::sanitize_terminal_inline};

use super::{
    args::ListAction,
    data::init_runtime,
    format::wrap_graph_permission_error,
};

pub(super) async fn run_list_action(config: &AppConfig, action: ListAction, as_json: bool) -> Result<()> {
    let (_, graph, _) = init_runtime(config, false).await?;
    let outcome: Result<()> = async {
        match action {
            ListAction::Ls { site_id } => {
                let lists = graph.list_lists(&site_id).await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "lists": lists,
                            "returned": lists.len(),
                        }))?
                    );
                } else {
                    for list in lists {
                        let display = list.display_name.as_deref().unwrap_or(&list.name);
                        println!(
                            "{}  {}",
                            sanitize_terminal_inline(&list.id),
                            sanitize_terminal_inline(display)
                        );
                    }
                }
            }
            ListAction::Items {
                target,
                fields,
                limit,
                cursor,
            } => {
                let page = graph
                    .list_items_page(
                        &target.site_id,
                        &target.list_id,
                        &fields,
                        usize::from(limit),
                        cursor.as_deref(),
                    )
                    .await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({
                            "items": page.items,
                            "returned": page.items.len(),
                            "has_more": page.next_cursor.is_some(),
                            "next_cursor": page.next_cursor,
                        }))?
                    );
                } else {
                    for item in &page.items {
                        let fields_json = serde_json::to_string(&item.fields)?;
                        println!(
                            "{}  {}  {}",
                            sanitize_terminal_inline(&item.id),
                            sanitize_terminal_inline(item.e_tag.as_deref().unwrap_or("")),
                            sanitize_terminal_inline(&fields_json)
                        );
                    }
                    if let Some(next) = &page.next_cursor {
                        eprintln!("More rows: rerun with --cursor {next}");
                    }
                }
            }
            ListAction::Columns { target } => {
                let columns = graph.list_columns(&target.site_id, &target.list_id).await?;
                if as_json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&json!({ "columns": columns }))?
                    );
                } else {
                    for column in columns {
                        let display = column.display_name.as_deref().unwrap_or(&column.name);
                        let access = if column.read_only == Some(true) {
                            "  (read-only)"
                        } else {
                            ""
                        };
                        println!(
                            "{}  {}{access}",
                            sanitize_terminal_inline(&column.name),
                            sanitize_terminal_inline(display)
                        );
                    }
                }
            }
            ListAction::SetField {
                target,
                item_id,
                field,
                value_json,
                if_match,
            } => {
                let update = graph
                    .update_list_item_field(
                        &target.site_id,
                        &target.list_id,
                        &item_id,
                        &field,
                        value_json,
                        if_match.as_deref(),
                    )
                    .await?;
                if as_json {
                    println!("{}", serde_json::to_string_pretty(&update)?);
                } else {
                    println!(
                        "Updated {} on row {}.",
                        sanitize_terminal_inline(&update.field),
                        sanitize_terminal_inline(&update.item_id)
                    );
                }
            }
        }
        Ok(())
    }
    .await;
    match outcome {
        Ok(()) => Ok(()),
        Err(err) => Err(wrap_graph_permission_error(&graph, err).await),
    }
}
