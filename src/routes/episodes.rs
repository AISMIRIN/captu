use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
};
use serde::{Deserialize, Deserializer};

use super::{AppState, HtmlTemplate};

fn empty_as_none_i64<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = Option::<String>::deserialize(d)?;
    match s.as_deref() {
        None | Some("") => Ok(None),
        Some(v) => v.parse::<i64>().map(Some).map_err(serde::de::Error::custom),
    }
}

pub struct EpisodeItem {
    pub episode_number: Option<i64>,
    pub episode_title: Option<String>,
    pub air_date: Option<String>,
}

pub struct SubtitleItem {
    pub title: String,
    pub air_date: Option<String>,
}

/// Fragment returned to htmx when the program selector changes.
/// If all episodes have no episode_number, shows a subtitle selector; otherwise shows an episode list.
#[derive(Template)]
#[template(path = "fragments/episodes.html")]
pub struct EpisodesTemplate {
    /// Some → episode selector; None → subtitle selector
    pub episodes: Option<Vec<EpisodeItem>>,
    /// Distinct subtitle values when episodes is None (all rows lack episode_number).
    pub subtitles: Option<Vec<SubtitleItem>>,
}

#[derive(Deserialize)]
pub struct EpisodesParams {
    #[serde(default, deserialize_with = "empty_as_none_i64")]
    pub program_id: Option<i64>,
}

pub async fn episodes(
    State(state): State<AppState>,
    Query(params): Query<EpisodesParams>,
) -> Result<HtmlTemplate<EpisodesTemplate>, StatusCode> {
    let pid = match params.program_id {
        Some(p) if p > 0 => p,
        _ => {
            return Ok(HtmlTemplate(EpisodesTemplate {
                episodes: Some(vec![]),
                subtitles: None,
            }));
        }
    };

    let rows = sqlx::query!(
        "SELECT episode_number, episode_title, air_date
         FROM ts_files
         WHERE program_id = ? AND status = 'done'
           AND EXISTS (SELECT 1 FROM captions c WHERE c.ts_file_id = ts_files.id)
         ORDER BY episode_number, air_date",
        pid,
    )
    .fetch_all(&state.pool)
    .await
    .map_err(|e| {
        tracing::error!("/api/episodes db error: {:#}", e);
        StatusCode::INTERNAL_SERVER_ERROR
    })?;

    let items: Vec<EpisodeItem> = rows
        .into_iter()
        .map(|r| EpisodeItem {
            episode_number: r.episode_number,
            episode_title: r.episode_title,
            // air_date is a DATE column; chrono decodes it as NaiveDate — convert to String.
            air_date: r.air_date.map(|d| d.to_string()),
        })
        .collect();

    // If every row lacks episode_number, show a subtitle selector instead.
    let all_null = items.iter().all(|e| e.episode_number.is_none());

    if all_null {
        let sub_rows = sqlx::query!(
            // MIN() on DATE can return NULL type in SQLite's type inference.
            // Annotate the inner type as String; the nullable column wraps it in Option<String>.
            r#"SELECT episode_title AS "episode_title!",
                      MIN(air_date) AS "air_date: String"
             FROM ts_files
             WHERE program_id = ? AND status = 'done' AND episode_title IS NOT NULL
               AND EXISTS (SELECT 1 FROM captions c WHERE c.ts_file_id = ts_files.id)
             GROUP BY episode_title
             ORDER BY air_date"#,
            pid,
        )
        .fetch_all(&state.pool)
        .await
        .map_err(|e| {
            tracing::error!("/api/episodes subtitles query: {:#}", e);
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

        let subtitles: Vec<SubtitleItem> = sub_rows
            .into_iter()
            .map(|r| SubtitleItem {
                title: r.episode_title,
                air_date: r.air_date,
            })
            .collect();

        Ok(HtmlTemplate(EpisodesTemplate {
            episodes: None,
            subtitles: if subtitles.is_empty() {
                None
            } else {
                Some(subtitles)
            },
        }))
    } else {
        Ok(HtmlTemplate(EpisodesTemplate {
            episodes: Some(items),
            subtitles: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::empty_as_none_i64;
    use serde::de::IntoDeserializer;
    use serde_json::Value as JValue;

    fn de_i64(opt: Option<&str>) -> Option<i64> {
        let v: JValue = match opt {
            None => JValue::Null,
            Some(s) => JValue::String(s.to_string()),
        };
        empty_as_none_i64(v.into_deserializer()).expect("deserialize failed")
    }

    #[test]
    fn absent_gives_none() {
        assert_eq!(de_i64(None), None);
    }

    #[test]
    fn empty_string_gives_none() {
        assert_eq!(de_i64(Some("")), None);
    }

    #[test]
    fn valid_number() {
        assert_eq!(de_i64(Some("42")), Some(42));
    }

    #[test]
    fn negative_number() {
        assert_eq!(de_i64(Some("-1")), Some(-1));
    }

    #[test]
    fn parse_error_propagates() {
        let v: JValue = JValue::String("abc".to_string());
        assert!(empty_as_none_i64(v.into_deserializer()).is_err());
    }
}
