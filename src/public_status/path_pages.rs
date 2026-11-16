//! Path-per-page public routing: every status page on the operator host at
//! `/{slug}`, for a single-tenant deploy that runs many pages without giving
//! each a host of its own.
//!
//! Off unless `tenancy.path_page_routes` is set alongside the path-based
//! surface (see [`crate::config::TenancyConfig::path_page_routes_active`]).
//!
//! The handlers are not taught about the prefix. Translation happens at the
//! edge, in one place:
//!
//! * **inbound** — [`wrap`] sits outside the app router. A request whose first
//!   segment is an enabled page's slug is rewritten onto the existing public
//!   routes (`/{slug}` → `/status`, `/{slug}/incidents/…` → `/status/incidents/…`,
//!   `/{slug}/access/…` → `/access/…`, …) and the page is pinned for it in
//!   [`PAGE_HEADER`], which [`crate::request::host::resolve_status_page`]
//!   reads in place of the lone org's default page.
//! * **outbound** — links the app emits go through [`local`], which re-adds
//!   the prefix while the request is in scope; `Location` and the private-page
//!   cookies' `Path` are rewritten on the way out, so redirects and sign-ins
//!   stay on the page they were made for.
//!
//! The bare `/status` surface keeps serving the default (oldest) page, so
//! links handed out before the switch keep working.
//!
//! A slug wins over nothing: a first segment that fails
//! [`crate::domain::validate_slug`] (which rejects every top-level app path,
//! see `domain::reserved_slugs`) never reaches the database, and the app's own
//! routes are reserved words a page cannot take.

use std::convert::Infallible;
use std::time::Duration;

use axum::Router;
use axum::extract::Request;
use axum::http::header::{LOCATION, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use moka::future::Cache;
use sqlx::PgPool;
use tower::ServiceExt;
use tower_cookies::cookie::Cookie;
use uuid::Uuid;

use crate::domain::{OrgId, PageRef, StatusPageId};

/// Internal request header carrying the page [`wrap`] resolved, as
/// `{page_id}:{org_id}`. Stripped from every incoming request before routing,
/// so a client can never pick a page by sending it.
pub const PAGE_HEADER: &str = "x-uptimepage-path-page";

/// The private-page cookies (`_up_page`, `_up_page_code`, `_up_page_confirm`).
/// Scoped to the page's prefix so hundreds of pages on one host keep separate
/// sign-ins.
const PAGE_COOKIE_PREFIX: &str = "_up_page";

/// How long a slug → page answer (including "no such page") is reused. Matches
/// the public page cache, so a new or renamed page shows up as fast as an edit
/// to an existing one does.
const LOOKUP_TTL: Duration = Duration::from_secs(10);
const LOOKUP_CAPACITY: u64 = 10_000;

/// Routes that keep their own name under the prefix. Everything else under a
/// page is a `/status/…` route with the `/status` dropped.
const PASSTHROUGH: &[&str] = &["/access", "/subscribe", "/api/public/v1"];

tokio::task_local! {
    /// `/{slug}` while a rewritten request is being handled.
    static PREFIX: String;
}

fn has_segment_prefix(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// The app route a path under a page's prefix maps onto, or `None` when the
/// result would not be a public route (so `/{slug}/settings` can never reach
/// the operator surface).
///
/// `/status/…` under the prefix is accepted as-is too, so a link built from an
/// origin that already carries the prefix (subscriber mail, see
/// [`crate::public_status::urls::page_root`]) still lands.
pub fn inbound(rest: &str) -> Option<String> {
    let mapped = if rest.is_empty() || rest == "/" {
        "/status".to_owned()
    } else if has_segment_prefix(rest, "/status")
        || PASSTHROUGH.iter().any(|p| has_segment_prefix(rest, p))
    {
        rest.to_owned()
    } else {
        format!("/status{rest}")
    };
    let public = has_segment_prefix(&mapped, "/status")
        || has_segment_prefix(&mapped, "/subscribe")
        || mapped.starts_with("/access/")
        || mapped.starts_with("/api/public/v1/");
    public.then_some(mapped)
}

/// `link` as seen from under `prefix`: the inverse of [`inbound`]. Leaves
/// anything that is not a public page route (absolute URLs, `/static/…`,
/// `/m/…`, already-prefixed paths) untouched, so applying it twice is
/// harmless.
pub fn with_prefix(prefix: &str, link: &str) -> String {
    if prefix.is_empty() || !link.starts_with('/') || link.starts_with("//") {
        return link.to_owned();
    }
    let split = link.find(['?', '#']).unwrap_or(link.len());
    let (path, tail) = link.split_at(split);
    if has_segment_prefix(path, "/status") {
        let rest = &path["/status".len()..];
        return format!("{prefix}{rest}{tail}");
    }
    if PASSTHROUGH.iter().any(|p| has_segment_prefix(path, p)) {
        return format!("{prefix}{path}{tail}");
    }
    link.to_owned()
}

/// `link` for the page the current request is serving: prefixed under
/// path-per-page routing, unchanged everywhere else (other surfaces, background
/// jobs, operator requests).
pub fn local(link: &str) -> String {
    PREFIX
        .try_with(|prefix| with_prefix(prefix, link))
        .unwrap_or_else(|_| link.to_owned())
}

/// `/{slug}` while serving a page under path-per-page routing, else empty. For
/// links that leave the request task (spawned mail) before [`local`] could run.
pub fn prefix() -> String {
    PREFIX.try_with(Clone::clone).unwrap_or_default()
}

/// The page [`wrap`] pinned for this request, if any. Only meaningful when
/// path-per-page routing is active, which is when [`wrap`] strips the header
/// from incoming requests; callers gate on that.
pub fn from_headers(headers: &HeaderMap) -> Option<PageRef> {
    let raw = headers.get(PAGE_HEADER)?.to_str().ok()?;
    let (page, org) = raw.split_once(':')?;
    Some(PageRef {
        page: StatusPageId(Uuid::parse_str(page).ok()?),
        org: OrgId(Uuid::parse_str(org).ok()?),
    })
}

/// First path segment as a candidate slug, plus the rest of the path. `None`
/// for anything that could not be a page's slug, which keeps every app route
/// (all reserved) away from the lookup.
fn split_slug(path: &str) -> Option<(String, &str)> {
    let tail = path.strip_prefix('/')?;
    let (segment, rest) = match tail.find('/') {
        Some(i) => tail.split_at(i),
        None => (tail, ""),
    };
    let slug = segment.to_ascii_lowercase();
    crate::domain::validate_slug(&slug).ok()?;
    Some((slug, rest))
}

#[derive(Clone)]
enum Source {
    Pg(Option<PgPool>),
    #[cfg(test)]
    Fixed(std::sync::Arc<Vec<(String, PageRef)>>),
}

#[derive(Clone)]
struct Resolver {
    source: Source,
    cache: Cache<String, Option<PageRef>>,
}

impl Resolver {
    fn new(source: Source) -> Self {
        Self {
            source,
            cache: Cache::builder()
                .max_capacity(LOOKUP_CAPACITY)
                .time_to_live(LOOKUP_TTL)
                .build(),
        }
    }

    async fn lookup(&self, slug: &str) -> Option<PageRef> {
        if let Some(hit) = self.cache.get(slug).await {
            return hit;
        }
        let found = match &self.source {
            Source::Pg(pool) => match find_page(pool.as_ref()?, slug).await {
                Ok(found) => found,
                Err(e) => {
                    // Not cached: the next request retries rather than 404ing
                    // a live page for the TTL.
                    tracing::warn!(error = %e, "path page lookup failed");
                    return None;
                }
            },
            #[cfg(test)]
            Source::Fixed(pages) => pages.iter().find(|(s, _)| s == slug).map(|(_, page)| *page),
        };
        self.cache.insert(slug.to_owned(), found).await;
        found
    }
}

/// An enabled, unheld page of the lone live org. The single-org check keeps the
/// path surface the single-tenant surface it is: a second org's pages are never
/// served on this host.
async fn find_page(pool: &PgPool, slug: &str) -> crate::error::Result<Option<PageRef>> {
    let Some(org) = crate::storage::orgs::find_lone_active_org(pool).await? else {
        return Ok(None);
    };
    Ok(
        crate::storage::orgs::find_public_status_page_by_slug(pool, slug)
            .await?
            .map(|resolved| resolved.0)
            .filter(|page| page.org == org),
    )
}

/// Put path-per-page routing in front of the app router. Everything that is not
/// under a page's slug passes through untouched (minus [`PAGE_HEADER`]).
pub fn wrap(inner: Router, pool: Option<PgPool>) -> Router {
    wrap_with(inner, Resolver::new(Source::Pg(pool)))
}

fn wrap_with(inner: Router, resolver: Resolver) -> Router {
    let service = tower::service_fn(move |req: Request| {
        let inner = inner.clone();
        let resolver = resolver.clone();
        async move { Ok::<_, Infallible>(dispatch(inner, resolver, req).await) }
    });
    Router::new().fallback_service(service)
}

async fn dispatch(inner: Router, resolver: Resolver, mut req: Request) -> Response {
    req.headers_mut().remove(PAGE_HEADER);
    let path = req.uri().path().to_owned();
    let Some((slug, rest)) = split_slug(&path) else {
        return call(inner, req).await;
    };
    let Some(page) = resolver.lookup(&slug).await else {
        return call(inner, req).await;
    };
    let Some(mapped) = inbound(rest) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let path_and_query = match req.uri().query() {
        Some(q) => format!("{mapped}?{q}"),
        None => mapped,
    };
    let mut parts = req.uri().clone().into_parts();
    parts.path_and_query = match path_and_query.parse() {
        Ok(pq) => Some(pq),
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    *req.uri_mut() = match Uri::from_parts(parts) {
        Ok(uri) => uri,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let pinned = format!("{}:{}", page.page.0, page.org.0);
    match HeaderValue::from_str(&pinned) {
        Ok(v) => {
            req.headers_mut().insert(PAGE_HEADER, v);
        }
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }

    let prefix = format!("/{slug}");
    let mut resp = PREFIX.scope(prefix.clone(), call(inner, req)).await;
    rewrite_response(&prefix, resp.headers_mut());
    resp
}

async fn call(inner: Router, req: Request) -> Response {
    match inner.oneshot(req).await {
        Ok(resp) => resp,
        Err(never) => match never {},
    }
}

/// Keep redirects and the private-page cookies on the page they belong to.
fn rewrite_response(prefix: &str, headers: &mut HeaderMap) {
    let location = headers
        .get(LOCATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|loc| {
            let mapped = with_prefix(prefix, loc);
            (mapped != loc).then_some(mapped)
        });
    if let Some(mapped) = location
        && let Ok(v) = HeaderValue::from_str(&mapped)
    {
        headers.insert(LOCATION, v);
    }

    let cookies: Vec<HeaderValue> = headers
        .get_all(SET_COOKIE)
        .iter()
        .map(|v| rewrite_cookie(prefix, v))
        .collect();
    if !cookies.is_empty() {
        headers.remove(SET_COOKIE);
        for cookie in cookies {
            headers.append(SET_COOKIE, cookie);
        }
    }
}

fn rewrite_cookie(prefix: &str, value: &HeaderValue) -> HeaderValue {
    let Ok(raw) = value.to_str() else {
        return value.clone();
    };
    let Ok(mut cookie) = Cookie::parse(raw.to_owned()) else {
        return value.clone();
    };
    if !cookie.name().starts_with(PAGE_COOKIE_PREFIX) {
        return value.clone();
    }
    let path = cookie.path().unwrap_or("/").to_owned();
    let mapped = if path == "/" {
        prefix.to_owned()
    } else {
        with_prefix(prefix, &path)
    };
    cookie.set_path(mapped);
    HeaderValue::from_str(&cookie.to_string()).unwrap_or_else(|_| value.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_maps_page_paths_onto_public_routes() {
        assert_eq!(inbound("").as_deref(), Some("/status"));
        assert_eq!(inbound("/").as_deref(), Some("/status"));
        assert_eq!(inbound("/incidents").as_deref(), Some("/status/incidents"));
        assert_eq!(
            inbound("/incidents/0193").as_deref(),
            Some("/status/incidents/0193")
        );
        assert_eq!(
            inbound("/branding/logo").as_deref(),
            Some("/status/branding/logo")
        );
        assert_eq!(inbound("/status").as_deref(), Some("/status"));
        assert_eq!(
            inbound("/status/incidents").as_deref(),
            Some("/status/incidents")
        );
        assert_eq!(inbound("/access/code").as_deref(), Some("/access/code"));
        assert_eq!(inbound("/subscribe").as_deref(), Some("/subscribe"));
        assert_eq!(
            inbound("/subscribe/confirm").as_deref(),
            Some("/subscribe/confirm")
        );
        assert_eq!(
            inbound("/api/public/v1/status").as_deref(),
            Some("/api/public/v1/status")
        );
    }

    #[test]
    fn inbound_never_reaches_the_operator_surface() {
        // Mapped under /status, which has no such route: a 404, not the app.
        assert_eq!(inbound("/settings").as_deref(), Some("/status/settings"));
        assert_eq!(inbound("/access"), None);
        assert_eq!(inbound("/api/public/v1"), None);
    }

    #[test]
    fn with_prefix_is_the_inverse_of_inbound() {
        let p = "/acme";
        assert_eq!(with_prefix(p, "/status"), "/acme");
        assert_eq!(with_prefix(p, "/status?fragment=1"), "/acme?fragment=1");
        assert_eq!(with_prefix(p, "/status#history"), "/acme#history");
        assert_eq!(with_prefix(p, "/status/incidents/1"), "/acme/incidents/1");
        assert_eq!(
            with_prefix(p, "/status/incidents?show=incidents"),
            "/acme/incidents?show=incidents"
        );
        assert_eq!(
            with_prefix(p, "/status/branding/logo?v=ab"),
            "/acme/branding/logo?v=ab"
        );
        assert_eq!(with_prefix(p, "/access/verify"), "/acme/access/verify");
        assert_eq!(with_prefix(p, "/subscribe"), "/acme/subscribe");
        assert_eq!(
            with_prefix(p, "/api/public/v1/incidents.rss?t=x"),
            "/acme/api/public/v1/incidents.rss?t=x"
        );
        for rest in ["/", "/incidents/1", "/access/verify", "/subscribe"] {
            let out = with_prefix(p, &inbound(rest).unwrap());
            assert_eq!(inbound(out.strip_prefix(p).unwrap()), inbound(rest));
        }
    }

    #[test]
    fn with_prefix_leaves_everything_else_alone() {
        let p = "/acme";
        for link in [
            "https://example.com/status",
            "//example.com/status",
            "/static/css/app.css",
            "/m/abc",
            "/statuses",
            "/acme/incidents/1",
            "?fragment=1",
            "",
        ] {
            assert_eq!(with_prefix(p, link), link);
        }
        assert_eq!(with_prefix("", "/status"), "/status");
    }

    #[test]
    fn local_is_a_no_op_outside_a_page_request() {
        assert_eq!(local("/status/incidents/1"), "/status/incidents/1");
    }

    #[tokio::test]
    async fn local_prefixes_inside_a_page_request() {
        let out = PREFIX
            .scope("/acme".to_owned(), async { local("/status/incidents/1") })
            .await;
        assert_eq!(out, "/acme/incidents/1");
    }

    #[test]
    fn split_slug_skips_app_routes() {
        assert_eq!(split_slug("/acme-corp"), Some(("acme-corp".to_owned(), "")));
        assert_eq!(
            split_slug("/Acme-Corp/incidents/1"),
            Some(("acme-corp".to_owned(), "/incidents/1"))
        );
        for path in [
            "/",
            "/status",
            "/settings/pages",
            "/static/app.css",
            "/api/v1/targets",
            "/login",
            "/targets",
            "/access/code",
            "/subscribe",
            "/favicon.ico",
            "/m/token",
        ] {
            assert_eq!(split_slug(path), None, "{path}");
        }
    }

    #[test]
    fn page_header_round_trips() {
        let page = PageRef {
            page: StatusPageId(Uuid::from_u128(3)),
            org: OrgId(Uuid::from_u128(4)),
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            PAGE_HEADER,
            HeaderValue::from_str(&format!("{}:{}", page.page.0, page.org.0)).unwrap(),
        );
        assert_eq!(from_headers(&headers), Some(page));
        headers.insert(PAGE_HEADER, HeaderValue::from_static("garbage"));
        assert_eq!(from_headers(&headers), None);
    }

    fn northwind() -> PageRef {
        PageRef {
            page: StatusPageId(Uuid::from_u128(1)),
            org: OrgId(Uuid::from_u128(2)),
        }
    }

    /// `northwind`, not `acme`: a fixture slug has to be one a page could
    /// actually hold, and `acme` is reserved.
    ///
    /// The real wrapper around a stand-in app router whose handlers report
    /// what they saw: the path they were routed on, the pinned page, and a
    /// link built with [`local`].
    fn app() -> Router {
        use axum::response::Redirect;
        use axum::routing::{get, post};

        async fn echo(uri: Uri, headers: HeaderMap) -> String {
            let pinned = from_headers(&headers).map_or("-".to_owned(), |p| p.page.0.to_string());
            format!("{uri} {pinned} {}", local("/status/incidents/9"))
        }
        let inner = Router::new()
            .route("/status", get(echo))
            .route("/status/incidents/{id}", get(echo))
            .route(
                "/status/go",
                get(|| async { Redirect::to("/status/incidents/1") }),
            )
            .route("/access/code", post(echo))
            .route("/login", get(|| async { "login" }));
        let pages = vec![("northwind".to_owned(), northwind())];
        wrap_with(
            inner,
            Resolver::new(Source::Fixed(std::sync::Arc::new(pages))),
        )
    }

    async fn send(method: &str, uri: &str, spoof: bool) -> (StatusCode, HeaderMap, String) {
        let mut req = axum::http::Request::builder().method(method).uri(uri);
        if spoof {
            req = req.header(
                PAGE_HEADER,
                format!("{}:{}", Uuid::from_u128(7), Uuid::from_u128(8)),
            );
        }
        let resp = app()
            .oneshot(req.body(axum::body::Body::empty()).unwrap())
            .await
            .unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn a_page_slug_is_served_from_the_public_routes() {
        let page = northwind().page.0;
        let (status, _, body) = send("GET", "/northwind", false).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, format!("/status {page} /northwind/incidents/9"));

        let (_, _, body) = send("GET", "/Northwind/incidents/5?show=all", false).await;
        assert_eq!(
            body,
            format!("/status/incidents/5?show=all {page} /northwind/incidents/9")
        );

        let (status, _, body) = send("POST", "/northwind/access/code", false).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("/access/code "), "{body}");
    }

    #[tokio::test]
    async fn redirects_stay_under_the_page() {
        let (status, headers, _) = send("GET", "/northwind/go", false).await;
        assert!(status.is_redirection());
        assert_eq!(headers.get(LOCATION).unwrap(), "/northwind/incidents/1");
    }

    #[tokio::test]
    async fn everything_else_passes_through_untouched() {
        let (_, _, body) = send("GET", "/login", false).await;
        assert_eq!(body, "login");
        // The bare surface keeps its own links and no pinned page.
        let (_, _, body) = send("GET", "/status", false).await;
        assert_eq!(body, "/status - /status/incidents/9");
        let (status, _, _) = send("GET", "/ghost-page", false).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // A page path that is not a public route never reaches the app.
        let (status, _, _) = send("GET", "/northwind/access", false).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn a_client_cannot_pin_a_page() {
        let (_, _, body) = send("GET", "/status", true).await;
        assert_eq!(body, "/status - /status/incidents/9");
        let (_, _, body) = send("GET", "/northwind", true).await;
        assert!(body.contains(&northwind().page.0.to_string()), "{body}");
    }

    #[test]
    fn page_cookies_are_scoped_to_the_page() {
        let mut headers = HeaderMap::new();
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("_up_page=v; HttpOnly; Path=/; Max-Age=60"),
        );
        headers.append(
            SET_COOKIE,
            HeaderValue::from_static("_up_page_confirm=n; Path=/access/verify"),
        );
        headers.append(SET_COOKIE, HeaderValue::from_static("other=1; Path=/"));
        headers.insert(LOCATION, HeaderValue::from_static("/status/incidents/1"));
        rewrite_response("/acme", &mut headers);

        let cookies: Vec<String> = headers
            .get_all(SET_COOKIE)
            .iter()
            .map(|v| v.to_str().unwrap().to_owned())
            .collect();
        assert!(cookies[0].starts_with("_up_page=v") && cookies[0].contains("Path=/acme"));
        assert!(cookies[0].contains("HttpOnly"));
        assert!(cookies[1].contains("Path=/acme/access/verify"));
        assert_eq!(cookies[2], "other=1; Path=/");
        assert_eq!(headers.get(LOCATION).unwrap(), "/acme/incidents/1");
    }
}
