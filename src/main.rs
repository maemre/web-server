use std::collections::HashMap;
use std::error::Error;
use std::sync::Arc;

use axum::{
    Router,
    extract::{Form, Path, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use minijinja::{Environment, context};
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

// Data model

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Bookmark {
    id: u64,
    url: String,
    title: String,
    tags: Vec<String>,
}

/// HTML form fields sent by the browser on POST /bookmarks.
#[derive(Debug, Deserialize)]
struct CreateBookmarkForm {
    url: String,
    title: String,
    tags: Option<String>,
}

// Application state

#[derive(Clone)]
struct Model {
    pool: SqlitePool,
}

impl Model {
    async fn get_all_bookmarks(&self) -> Result<Vec<Bookmark>> {
        let bookmarks =
            sqlx::query_as::<_, (u64, String, String)>("select id, url, title from bookmark")
                .fetch_all(&self.pool)
                .await?;

        let links = sqlx::query_as::<_, (u64, u64)>(
            r"SELECT bookmark_id, tag_id FROM bookmark_tag"
        )
            .fetch_all(&self.pool)
            .await?;

        let tags = sqlx::query_as::<_, (u64, String)>(
            r"SELECT id, name FROM tag WHERE id IN (SELECT tag_id FROM bookmark_tag)",
        )
        .fetch_all(&self.pool)
        .await?
        .into_iter()
        .collect::<HashMap<_, _>>();

        let mut tags_by_bookmark: HashMap<u64, Vec<String>> = HashMap::new();
        for (bookmark_id, tag_id) in &links {
            if let Some(name) = tags.get(tag_id) {
                tags_by_bookmark
                    .entry(*bookmark_id)
                    .or_default()
                    .push(name.clone());
            }
        }

        let mut bookmarks: Vec<_> = bookmarks
            .into_iter()
            .map(|(id, url, title)| Bookmark {
                id,
                url,
                title,
                tags: tags_by_bookmark.remove(&id).unwrap_or_default(),
            })
            .collect();
        bookmarks.sort_by_key(|b| b.id);

        Ok(bookmarks)
    }

    async fn get_bookmark_from_id(&self, id: u64) -> Result<Option<Bookmark>> {
        let Some((id, url, title)) = sqlx::query_as::<_, (u64, String, String)>(
            "select id, url, title from bookmark where id = ?",
        )
        .bind(id as i64)
        .fetch_optional(&self.pool)
        .await?
        else {
            return Ok(None);
        };

        let tags = sqlx::query_scalar::<_, String>(
            r"select tag.name from tag, bookmark_tag bt
              where tag.id = bt.tag_id and bt.bookmark_id = ?",
        )
        .bind(id as i64)
        .fetch_all(&self.pool)
        .await?;
        Ok(Some(Bookmark {
            id,
            url,
            title,
            tags,
        }))
    }

    async fn create_bookmark(&self, url: String, title: String, tags: Vec<String>) -> Result<i64> {
        let mut trans = self.pool.begin().await?;

        let bookmark_id = sqlx::query_scalar::<_, i64>(
            r"INSERT INTO bookmark (url, title) VALUES (?, ?) RETURNING id",
        )
        .bind(url)
        .bind(title)
        .fetch_one(&mut *trans)
        .await?;

        // create the tags
        let placeholders = vec!["(?)"; tags.len()].join(", ");
        let query_text = format!(r"insert or ignore into tag (name) values {placeholders}");
        let insert_query = tags
            .iter()
            .fold(sqlx::query(&query_text), |query, tag| query.bind(tag));
        insert_query.execute(&mut *trans).await?;

        // create the links
        let placeholders = vec!["?"; tags.len()].join(", ");
        let link_tags = format!(
            r"INSERT INTO bookmark_tag (bookmark_id, tag_id)
              SELECT ?, tag.id FROM tag WHERE tag.name IN ({placeholders})"
        );
        let mut q = sqlx::query(&link_tags).bind(bookmark_id);
        for tag in tags {
            q = q.bind(tag);
        }
        q.execute(&mut *trans).await?;

        trans.commit().await?;

        Ok(bookmark_id)
    }
}

/// Everything handlers need: the data store **and** the template engine.
///
/// We wrap the Environment in an Arc so it can be shared cheaply across
/// tasks.  It's immutable after setup, so no Mutex needed.
#[derive(Clone)]
struct AppState {
    model: Model,
    templates: Arc<Environment<'static>>,
}

// Create a database error response
fn database_error() -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Html("database error".to_string()),
    )
        .into_response()
}

// Templating

/// Builds the MiniJinja environment with all our templates.
///
/// MiniJinja uses Jinja2 syntax:
///   {{ variable }}         -- output a value
///   {% for x in xs %}      -- control flow
///   {% block name %}       -- template inheritance
///
/// We define templates inline for simplicity.  In a real project you'd
/// load them from disk (Environment::set_loader).
fn build_templates() -> Environment<'static> {
    let mut env = Environment::new();

    env.add_template("base.html", include_str!("../templates/base.html"))
        .unwrap();

    env.add_template("list.html", include_str!("../templates/list.html"))
        .unwrap();

    env.add_template("detail.html", include_str!("../templates/detail.html"))
        .unwrap();

    env.add_template("new.html", include_str!("../templates/new.html"))
        .unwrap();

    env.add_template("404.html", include_str!("../templates/404.html"))
        .unwrap();

    env
}

/// Renders a template or returns a 500 error page.
///
/// Centralises the boilerplate of "get template → render → wrap in Html".
fn render(env: &Environment, name: &str, ctx: minijinja::Value) -> Response {
    match env.get_template(name).and_then(|t| t.render(ctx)) {
        Ok(html) => Html(html).into_response(),
        Err(e) => {
            eprintln!("template error: {e:#}");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Html("template error".to_string()),
            )
                .into_response()
        }
    }
}

// Handlers

/// GET /bookmarks
async fn list_bookmarks(State(state): State<AppState>) -> Response {
    let Ok(bookmarks) = state.model.get_all_bookmarks().await else {
        return database_error();
    };
    render(&state.templates, "list.html", context! { bookmarks })
}

/// GET /bookmarks/new
///
/// Note: this route is registered **before** `/bookmarks/:id` so that the
/// literal path "new" isn't captured as an id.
async fn new_bookmark_form(State(state): State<AppState>) -> Response {
    render(&state.templates, "new.html", context! {})
}

/// GET /bookmarks/:id
async fn get_bookmark(State(state): State<AppState>, Path(id): Path<u64>) -> Response {
    match state.model.get_bookmark_from_id(id).await {
        Err(_) => database_error(),
        Ok(Some(bm)) => render(&state.templates, "detail.html", context! { bookmark => bm }),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            render(&state.templates, "404.html", context! {}),
        )
            .into_response(),
    }
}

/// POST /bookmarks
async fn create_bookmark(
    State(state): State<AppState>,
    Form(form): Form<CreateBookmarkForm>,
) -> Response {
    let tags: Vec<String> = form
        .tags
        .unwrap_or_default()
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();

    let Ok(id) = state.model.create_bookmark(form.url, form.title, tags).await else {
        return database_error();
    };

    Redirect::to(&format!("/bookmarks/{id}")).into_response()
}

fn build_router(state: AppState) -> Router {
    // Important: `/bookmarks/new` must be registered before `/bookmarks/:id`
    // so that "new" isn't interpreted as an id parameter.
    Router::new()
        .route("/bookmarks", get(list_bookmarks).post(create_bookmark))
        .route("/bookmarks/new", get(new_bookmark_form))
        .route("/bookmarks/{id}", get(get_bookmark))
        .with_state(state)
}

// Main

#[tokio::main]
async fn main() {
    let pool = SqlitePool::connect("sqlite:bookmarks.db?mode=rwc")
        .await
        .expect("Cannot connect to the database");
    sqlx::raw_sql(include_str!("../schema.sql"))
        .execute(&pool)
        .await
        .expect("Cannot create the schema");

    let state = AppState {
        model: Model { pool },
        templates: Arc::new(build_templates()),
    };

    let app = build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080")
        .await
        .expect("failed to bind port 8080");

    println!("Open http://127.0.0.1:8080/bookmarks in your browser");
    axum::serve(listener, app).await.expect("server error");
}
