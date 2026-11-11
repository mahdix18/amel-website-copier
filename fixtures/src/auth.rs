//! The `auth` site: a form login guarded by a CSRF token, a members area
//! behind a session cookie, a logout link that must never be followed, and
//! an HTTP Basic area. All credentials here are fake, for tests only.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::{Form, Router};
use serde::Deserialize;

pub const USERNAME: &str = "ann";
pub const PASSWORD: &str = "secret";
/// The session cookie a successful login sets, as `name=value`.
pub const SESSION_COOKIE: &str = "session=member-7f3a";
/// `Authorization` value for `USERNAME:PASSWORD`.
const BASIC_AUTH: &str = "Basic YW5uOnNlY3JldA==";

struct Site {
    /// Differs per server, so a login has to read it from the form.
    csrf: String,
}

#[derive(Deserialize)]
struct LoginForm {
    csrf: String,
    username: String,
    password: String,
}

pub(crate) fn router() -> Router {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    let site = Arc::new(Site {
        csrf: format!("csrf-{nanos:x}"),
    });
    Router::new()
        .route("/", get(index))
        .route(
            "/robots.txt",
            get(|| async { include_str!("../sites/auth/robots.txt") }),
        )
        .route("/login", get(login_form).post(log_in))
        .route("/logout", get(log_out))
        .route(
            "/members/",
            get(|headers: HeaderMap| async move { members(&headers, MEMBERS_HOME) }),
        )
        .route(
            "/members/page2.html",
            get(|headers: HeaderMap| async move { members(&headers, MEMBERS_PAGE2) }),
        )
        .route(
            "/basic/",
            get(|headers: HeaderMap| async move { basic(&headers, BASIC_HOME) }),
        )
        .route(
            "/basic/doc.html",
            get(|headers: HeaderMap| async move { basic(&headers, BASIC_DOC) }),
        )
        .with_state(site)
}

const MEMBERS_HOME: &str = r#"<!doctype html><title>Members</title>
<h1>Members area</h1><p class="welcome">Welcome back, ann</p>
<a href="page2.html">Page 2</a> <a href="/logout">Log out</a>"#;
const MEMBERS_PAGE2: &str = r#"<!doctype html><title>Members page 2</title>
<h1>Members page 2</h1><a href="./">Back</a> <a href="/logout">Log out</a>"#;
const BASIC_HOME: &str = r#"<!doctype html><title>Basic</title>
<h1>Basic area</h1><a href="doc.html">A document</a>"#;
const BASIC_DOC: &str = "<!doctype html><title>Doc</title><h1>Basic document</h1>";

async fn index() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>MisterCopy fixture: auth</title>
<a href="/login">Log in</a> <a href="/members/">Members</a> <a href="/basic/">Basic</a>"#,
    )
}

async fn login_form(State(site): State<Arc<Site>>) -> Html<String> {
    Html(format!(
        r#"<!doctype html><title>Log in</title>
<form method="post" action="/login">
  <input type="hidden" name="csrf" value="{}">
  <label>User <input id="username" name="username"></label>
  <label>Password <input id="password" name="password" type="password"></label>
  <button type="submit">Log in</button>
</form>"#,
        site.csrf
    ))
}

async fn log_in(State(site): State<Arc<Site>>, Form(form): Form<LoginForm>) -> Response {
    if form.csrf != site.csrf {
        return (StatusCode::FORBIDDEN, "stale form").into_response();
    }
    if form.username != USERNAME || form.password != PASSWORD {
        return (StatusCode::FORBIDDEN, "wrong user or password").into_response();
    }
    (
        StatusCode::SEE_OTHER,
        [
            (
                header::SET_COOKIE,
                format!("{SESSION_COOKIE}; Path=/; HttpOnly"),
            ),
            (header::LOCATION, "/members/".to_owned()),
        ],
    )
        .into_response()
}

async fn log_out() -> Response {
    (
        StatusCode::FOUND,
        [
            (header::SET_COOKIE, "session=; Path=/; Max-Age=0"),
            (header::LOCATION, "/login"),
        ],
    )
        .into_response()
}

fn members(headers: &HeaderMap, page: &'static str) -> Response {
    let logged_in = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|cookies| cookies.split(';').any(|c| c.trim() == SESSION_COOKIE));
    if logged_in {
        Html(page).into_response()
    } else {
        (StatusCode::FOUND, [(header::LOCATION, "/login")]).into_response()
    }
}

fn basic(headers: &HeaderMap, page: &'static str) -> Response {
    if headers
        .get(header::AUTHORIZATION)
        .is_some_and(|v| v == BASIC_AUTH)
    {
        Html(page).into_response()
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"fixture\"")],
            "login required",
        )
            .into_response()
    }
}
