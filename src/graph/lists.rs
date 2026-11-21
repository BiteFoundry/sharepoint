//! SharePoint list rows and column definitions through Microsoft Graph v1.0.

use std::collections::HashSet;

use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use super::{client::GraphListPage, GraphClient};

#[derive(Debug, Deserialize, Serialize)]
pub struct ListColumn {
    pub id: String,
    pub name: String,
    #[serde(rename(deserialize = "displayName"))]
    pub display_name: Option<String>,
    #[serde(rename(deserialize = "readOnly"))]
    pub read_only: Option<bool>,
    pub hidden: Option<bool>,
}

#[derive(Debug, Deserialize, Serialize)]
pub struct ListItem {
    pub id: String,
    #[serde(rename(deserialize = "eTag"))]
    pub e_tag: Option<String>,
    #[serde(rename = "@odata.etag", skip_serializing)]
    odata_etag: Option<String>,
    #[serde(default)]
    pub fields: Map<String, Value>,
}

#[derive(Debug, Serialize)]
pub struct ListFieldUpdate {
    pub item_id: String,
    pub field: String,
    pub e_tag: Option<String>,
    pub fields: Map<String, Value>,
}

#[derive(Debug, Serialize)]
pub struct ListItemsPage {
    pub items: Vec<ListItem>,
    pub next_cursor: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ListCursor {
    v: u8,
    site_id: String,
    list_id: String,
    fields: Vec<String>,
    next: String,
}

fn normalized_id(value: &str) -> String {
    value.trim().to_ascii_lowercase()
}

fn normalized_fields(fields: &[String]) -> Vec<String> {
    let mut fields = fields.to_vec();
    fields.sort();
    fields.dedup();
    fields
}

fn describe_fields(fields: &[String]) -> String {
    if fields.is_empty() {
        "all fields".to_string()
    } else {
        format!("fields [{}]", fields.join(", "))
    }
}

impl GraphClient {
    pub async fn list_columns(&self, site_id: &str, list_id: &str) -> Result<Vec<ListColumn>> {
        let path = list_path(site_id, list_id)?;
        self.get_list_collection(format!(
            "{path}/columns?$select=id,name,displayName,readOnly,hidden"
        ))
        .await
    }

    pub async fn list_items_page(
        &self,
        site_id: &str,
        list_id: &str,
        fields: &[String],
        limit: usize,
        cursor: Option<&str>,
    ) -> Result<ListItemsPage> {
        let path = list_path(site_id, list_id)?;
        for field in fields {
            ensure!(
                !field.is_empty() && field.chars().all(|ch| ch == '_' || ch.is_alphanumeric()),
                "Invalid internal column name {field:?}"
            );
        }
        let fields = normalized_fields(fields);
        let site_key = normalized_id(site_id);
        let list_key = normalized_id(list_id);
        let url = match cursor {
            Some(cursor) => self.checked_list_cursor(cursor, &site_key, &list_key, &fields)?,
            None => {
                let expand = if fields.is_empty() {
                    "fields".to_string()
                } else {
                    format!("fields(select={})", fields.join(","))
                };
                format!("{path}/items?expand={}&$top={limit}", urlencoding::encode(&expand))
            }
        };
        let page: GraphListPage<ListItem> = self.get_json(&url).await?;
        let mut items = page.value;
        for item in &mut items {
            item.e_tag = item.e_tag.take().or_else(|| item.odata_etag.take());
            item.fields.remove("@odata.etag");
            if !fields.is_empty() {
                item.fields.retain(|key, _| fields.contains(key));
            }
        }
        let next_cursor = match page.next_link {
            Some(next) => Some(URL_SAFE_NO_PAD.encode(serde_json::to_vec(&ListCursor {
                v: 1,
                site_id: site_key,
                list_id: list_key,
                fields,
                next,
            })?)),
            None => None,
        };
        Ok(ListItemsPage { items, next_cursor })
    }

    fn checked_list_cursor(
        &self,
        cursor: &str,
        site_key: &str,
        list_key: &str,
        fields: &[String],
    ) -> Result<String> {
        let invalid = || anyhow!("Invalid cursor: pass the next_cursor value from the previous page");
        let bytes = URL_SAFE_NO_PAD
            .decode(cursor.trim())
            .map_err(|_| invalid())?;
        let envelope: ListCursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        ensure!(envelope.v == 1, invalid());
        ensure!(
            envelope.site_id == site_key && envelope.list_id == list_key,
            "Cursor belongs to a different site_id or list_id"
        );
        ensure!(
            envelope.fields == fields,
            "Cursor was issued for {}; pass the same fields",
            describe_fields(&envelope.fields)
        );
        let url = Url::parse(&envelope.next).map_err(|_| invalid())?;
        let base = Url::parse(&self.resolve_url("/"))?;
        ensure!(
            url.origin() == base.origin(),
            "Cursor does not belong to the configured Graph endpoint"
        );
        let path = urlencoding::decode(url.path()).map_err(|_| invalid())?;
        let prefix = format!("{}/sites/", base.path().trim_end_matches('/'));
        ensure!(
            path.starts_with(&prefix) && path.ends_with("/items"),
            "Cursor is not a SharePoint list items page"
        );
        Ok(envelope.next)
    }

    pub async fn update_list_item_field(
        &self,
        site_id: &str,
        list_id: &str,
        item_id: &str,
        field: &str,
        value: Value,
        if_match: Option<&str>,
    ) -> Result<ListFieldUpdate> {
        let path = list_path(site_id, list_id)?;
        let item_id = item_id.trim();
        let item_segment = resource_id("item_id", item_id)?;
        let headers = match if_match {
            Some(tag) => {
                ensure!(!tag.trim().is_empty(), "If-Match requires a non-empty ETag");
                vec![("If-Match", tag.to_string())]
            }
            None => Vec::new(),
        };

        let column = self
            .list_columns(site_id, list_id)
            .await?
            .into_iter()
            .find(|column| column.name == field)
            .ok_or_else(|| anyhow!("Unknown internal column name {field:?}. Use the list's columns to find its name."))?;
        ensure!(
            column.read_only != Some(true),
            "Column {field:?} is read-only"
        );
        let body = Map::from_iter([(column.name, value)]);
        let response = self
            .patch_json_response(
                &format!("{path}/items/{item_segment}/fields"),
                &body,
                &headers,
            )
            .await?;
        let header_etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|tag| tag.to_str().ok())
            .map(str::to_owned);
        let mut fields: Map<String, Value> = response
            .json()
            .await
            .context("Graph accepted the field update but returned an invalid fieldValueSet")?;
        let body_etag = fields.remove("@odata.etag");
        fields.remove("@odata.context");
        Ok(ListFieldUpdate {
            item_id: item_id.to_string(),
            field: field.to_string(),
            e_tag: header_etag
                .or_else(|| body_etag.and_then(|tag| tag.as_str().map(str::to_owned))),
            fields,
        })
    }

    async fn get_list_collection<T: DeserializeOwned>(&self, first: String) -> Result<Vec<T>> {
        let origin = Url::parse(&self.resolve_url(&first))?.origin();
        let mut visited = HashSet::new();
        let mut items = Vec::new();
        let mut next = Some(first);
        while let Some(link) = next {
            let url = self.resolve_url(&link);
            ensure!(
                Url::parse(&url)?.origin() == origin,
                "Graph returned a nextLink on a different origin"
            );
            ensure!(
                visited.insert(url.clone()),
                "Graph returned a repeated nextLink"
            );
            let response: GraphListPage<T> = self.get_json(&url).await?;
            items.extend(response.value);
            next = response.next_link;
        }
        Ok(items)
    }
}

fn list_path(site_id: &str, list_id: &str) -> Result<String> {
    let site = resource_id("site_id", site_id)?;
    let list = resource_id("list_id", list_id)?;
    Ok(format!("/sites/{site}/lists/{list}"))
}

fn resource_id<'a>(name: &str, value: &'a str) -> Result<std::borrow::Cow<'a, str>> {
    let value = value.trim();
    ensure!(
        !matches!(value, "" | "." | ".."),
        "{name} must be a resource ID"
    );
    Ok(urlencoding::encode(value))
}
