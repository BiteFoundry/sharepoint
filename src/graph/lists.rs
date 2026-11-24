//! SharePoint list rows and column definitions through Microsoft Graph v1.0.

use std::collections::HashSet;

use anyhow::{anyhow, ensure, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use serde_json::{Map, Value};
use url::Url;

use super::{client::GraphListPage, GraphClient};
pub use crate::models::ListColumn;

#[derive(Debug, Deserialize)]
struct RawListFacet {
    template: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawListInfo {
    id: String,
    #[serde(default)]
    name: String,
    #[serde(rename = "displayName")]
    display_name: Option<String>,
    #[serde(rename = "webUrl")]
    web_url: Option<String>,
    list: Option<RawListFacet>,
}

#[derive(Debug, Serialize)]
pub struct ListInfo {
    pub id: String,
    pub name: String,
    pub display_name: Option<String>,
    pub web_url: Option<String>,
    pub template: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawChoiceFacet {
    #[serde(default)]
    choices: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RawListColumn {
    id: String,
    name: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(rename = "readOnly", default)]
    read_only: Option<bool>,
    #[serde(default)]
    hidden: Option<bool>,
    #[serde(default)]
    required: Option<bool>,
    #[serde(default)]
    text: Option<Value>,
    #[serde(default)]
    number: Option<Value>,
    #[serde(default)]
    choice: Option<RawChoiceFacet>,
    #[serde(rename = "dateTime", default)]
    date_time: Option<Value>,
    #[serde(rename = "personOrGroup", default)]
    person_or_group: Option<Value>,
    #[serde(default)]
    boolean: Option<Value>,
    #[serde(default)]
    lookup: Option<Value>,
    #[serde(default)]
    currency: Option<Value>,
    #[serde(rename = "hyperlinkOrPicture", default)]
    hyperlink_or_picture: Option<Value>,
    #[serde(default)]
    calculated: Option<Value>,
    #[serde(default)]
    geolocation: Option<Value>,
    #[serde(default)]
    term: Option<Value>,
    #[serde(default)]
    thumbnail: Option<Value>,
    #[serde(rename = "contentApprovalStatus", default)]
    content_approval_status: Option<Value>,
}

impl RawListColumn {
    fn infer_column_type(&self) -> Option<String> {
        let inferred = if self.choice.is_some() {
            "choice"
        } else if self.text.is_some() {
            "text"
        } else if self.number.is_some() {
            "number"
        } else if self.date_time.is_some() {
            "dateTime"
        } else if self.person_or_group.is_some() {
            "personOrGroup"
        } else if self.boolean.is_some() {
            "boolean"
        } else if self.lookup.is_some() {
            "lookup"
        } else if self.currency.is_some() {
            "currency"
        } else if self.hyperlink_or_picture.is_some() {
            "hyperlinkOrPicture"
        } else if self.calculated.is_some() {
            "calculated"
        } else if self.geolocation.is_some() {
            "geolocation"
        } else if self.term.is_some() {
            "term"
        } else if self.thumbnail.is_some() {
            "thumbnail"
        } else if self.content_approval_status.is_some() {
            "contentApprovalStatus"
        } else {
            return None;
        };
        Some(inferred.to_string())
    }

    fn into_model(self) -> ListColumn {
        let column_type = self.infer_column_type();
        let description = self
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let choices = self
            .choice
            .map(|c| c.choices)
            .unwrap_or_default();
        ListColumn {
            id: self.id,
            name: self.name,
            display_name: self.display_name,
            description,
            read_only: self.read_only,
            hidden: self.hidden,
            required: self.required.unwrap_or(false),
            column_type,
            choices,
        }
    }
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
    pub async fn list_lists(&self, site_id: &str) -> Result<Vec<ListInfo>> {
        let site = resource_id("site_id", site_id)?;
        let raw: Vec<RawListInfo> = self
            .get_list_collection(format!(
                "/sites/{site}/lists?$select=id,name,displayName,webUrl,list"
            ))
            .await?;
        Ok(raw
            .into_iter()
            .map(|item| ListInfo {
                id: item.id,
                name: item.name,
                display_name: item.display_name,
                web_url: item.web_url,
                template: item.list.and_then(|facet| facet.template),
            })
            .collect())
    }

    pub async fn list_columns(&self, site_id: &str, list_id: &str) -> Result<Vec<ListColumn>> {
        let path = list_path(site_id, list_id)?;
        let raw: Vec<RawListColumn> = self.get_list_collection(format!("{path}/columns")).await?;
        Ok(raw.into_iter().map(RawListColumn::into_model).collect())
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::get, Json, Router};
    use serde_json::json;
    use tokio::net::TcpListener;

    use crate::{
        auth::{AuthManager, StoredTokens},
        config::AppConfig,
    };

    #[test]
    fn raw_list_column_infers_types_choices_description_and_required() {
        let samples = json!([
            {
                "id": "c1",
                "name": "Status",
                "displayName": "Review Status",
                "description": "Current workflow stage",
                "readOnly": false,
                "hidden": false,
                "required": true,
                "choice": { "choices": ["Draft", "In Review", "Approved"] }
            },
            {
                "id": "c2",
                "name": "Notes",
                "displayName": "Notes",
                "description": "   ",
                "text": { "allowMultipleLines": true }
            },
            {
                "id": "c3",
                "name": "Budget",
                "currency": { "locale": "en-US" }
            },
            {
                "id": "c4",
                "name": "Count",
                "number": { "decimalPlaces": "none" }
            },
            {
                "id": "c5",
                "name": "DueDate",
                "dateTime": { "format": "dateOnly" }
            },
            {
                "id": "c6",
                "name": "Owner",
                "personOrGroup": { "allowMultipleSelection": false }
            },
            {
                "id": "c7",
                "name": "Approved",
                "boolean": {}
            },
            {
                "id": "c8",
                "name": "Project",
                "lookup": { "listId": "list-99" }
            },
            {
                "id": "c9",
                "name": "ReferenceLink",
                "hyperlinkOrPicture": { "isPicture": false }
            }
        ]);
        let raw: Vec<RawListColumn> = serde_json::from_value(samples).unwrap();
        let cols: Vec<ListColumn> = raw.into_iter().map(RawListColumn::into_model).collect();

        assert_eq!(cols[0].description.as_deref(), Some("Current workflow stage"));
        assert!(cols[0].required);
        assert_eq!(cols[0].column_type.as_deref(), Some("choice"));
        assert_eq!(cols[0].choices, vec!["Draft", "In Review", "Approved"]);

        assert_eq!(cols[1].description, None);
        assert!(!cols[1].required);
        assert_eq!(cols[1].column_type.as_deref(), Some("text"));
        assert!(cols[1].choices.is_empty());

        assert_eq!(cols[2].column_type.as_deref(), Some("currency"));
        assert_eq!(cols[3].column_type.as_deref(), Some("number"));
        assert_eq!(cols[4].column_type.as_deref(), Some("dateTime"));
        assert_eq!(cols[5].column_type.as_deref(), Some("personOrGroup"));
        assert_eq!(cols[6].column_type.as_deref(), Some("boolean"));
        assert_eq!(cols[7].column_type.as_deref(), Some("lookup"));
        assert_eq!(cols[8].column_type.as_deref(), Some("hyperlinkOrPicture"));
    }

    #[tokio::test]
    async fn list_columns_fetches_rich_column_metadata_from_graph() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().route(
            "/v1.0/sites/site-1/lists/list-1/columns",
            get(|| async {
                Json(json!({
                    "value": [
                        {
                            "id": "col-status",
                            "name": "DocStatus",
                            "displayName": "Document Status",
                            "description": "Approval state for compliance docs",
                            "readOnly": false,
                            "hidden": false,
                            "required": true,
                            "choice": { "choices": ["Draft", "Published", "Archived"] }
                        },
                        {
                            "id": "col-owner",
                            "name": "DocOwner",
                            "displayName": "Owner",
                            "personOrGroup": {}
                        }
                    ]
                }))
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let cfg = AppConfig {
            graph_base_url: format!("{base}/v1.0"),
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token: "test-token".into(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".into(),
                tenant_id: "tid".into(),
                user_id: Some("u1".into()),
                user_display_name: None,
                user_email: None,
                auth_mode: "test".into(),
            },
        );
        let graph = GraphClient::new(cfg, auth).unwrap();
        let cols = graph.list_columns("site-1", "list-1").await.unwrap();
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[0].name, "DocStatus");
        assert_eq!(
            cols[0].description.as_deref(),
            Some("Approval state for compliance docs")
        );
        assert!(cols[0].required);
        assert_eq!(cols[0].column_type.as_deref(), Some("choice"));
        assert_eq!(cols[0].choices, vec!["Draft", "Published", "Archived"]);
        assert_eq!(cols[1].name, "DocOwner");
        assert!(!cols[1].required);
        assert_eq!(cols[1].column_type.as_deref(), Some("personOrGroup"));

        server.abort();
    }
}
