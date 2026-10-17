use crate::error::PromoteError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForgeKind {
    GitHub,
    GitLab,
}

impl ForgeKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::GitHub => "github",
            Self::GitLab => "gitlab",
        }
    }
}

#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }

    #[must_use]
    pub fn redact(&self, text: &str) -> String {
        if self.0.is_empty() {
            text.to_owned()
        } else {
            text.replace(&self.0, "***")
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("Secret(***)")
    }
}

#[derive(Debug, Clone)]
pub struct ForgeTarget {
    pub kind: ForgeKind,
    pub api_base: Option<String>,
    pub project: String,
    pub token: Secret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRef {
    pub host: String,
    pub project: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub forge: ForgeKind,
    pub url: String,
    pub number: u64,
    pub existing: bool,
}

pub fn parse_remote(url: &str) -> Result<RemoteRef, PromoteError> {
    let unknown = || PromoteError::UnknownForge(url.to_owned());
    let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(scheme, "https" | "http" | "ssh" | "git") {
            return Err(unknown());
        }
        rest.split_once('/').ok_or_else(unknown)?
    } else {
        let (authority, path) = url.split_once(':').ok_or_else(unknown)?;
        if authority.contains('/') || !authority.contains('@') {
            return Err(unknown());
        }
        (authority, path)
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .split(':')
        .next()
        .unwrap_or_default();
    let project = path.trim_matches('/').trim_end_matches(".git");
    if host.is_empty() || !project.contains('/') {
        return Err(unknown());
    }
    Ok(RemoteRef {
        host: host.to_ascii_lowercase(),
        project: project.to_owned(),
    })
}

fn forge_error(secret: &Secret, error: &dyn std::fmt::Display) -> PromoteError {
    PromoteError::Forge(secret.redact(&error.to_string()))
}

pub async fn open_request(
    target: &ForgeTarget,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<Published, PromoteError> {
    match target.kind {
        ForgeKind::GitHub => github(target, head, base, title, body).await,
        ForgeKind::GitLab => gitlab(target, head, base, title, body).await,
    }
}

fn github_client(target: &ForgeTarget) -> Result<octocrab::Octocrab, octocrab::Error> {
    let mut builder =
        octocrab::Octocrab::builder().personal_token(target.token.expose().to_owned());
    if let Some(base_uri) = &target.api_base {
        builder = builder.base_uri(base_uri.as_str())?;
    }
    builder.build()
}

async fn github(
    target: &ForgeTarget,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<Published, PromoteError> {
    let fail = |error: octocrab::Error| forge_error(&target.token, &error);
    let (owner, repo) = target
        .project
        .split_once('/')
        .ok_or_else(|| PromoteError::UnknownForge(target.project.clone()))?;
    let client = github_client(target).map_err(fail)?;
    let found = client
        .pulls(owner, repo)
        .list()
        .state(octocrab::params::State::Open)
        .head(format!("{owner}:{head}"))
        .per_page(1)
        .send()
        .await
        .map_err(fail)?;
    let (pull, existing) = match found.items.into_iter().next() {
        Some(pull) => (pull, true),
        None => (
            client
                .pulls(owner, repo)
                .create(title, head, base)
                .body(body)
                .send()
                .await
                .map_err(fail)?,
            false,
        ),
    };
    Ok(Published {
        forge: ForgeKind::GitHub,
        url: pull.html_url.map(|url| url.to_string()).unwrap_or_default(),
        number: pull.number,
        existing,
    })
}

#[derive(serde::Deserialize)]
struct MergeRequestInfo {
    iid: u64,
    web_url: String,
}

async fn gitlab(
    target: &ForgeTarget,
    head: &str,
    base: &str,
    title: &str,
    body: &str,
) -> Result<Published, PromoteError> {
    use gitlab::api::{
        AsyncQuery, Pagination, merge_requests::MergeRequestState, paged, projects::merge_requests,
    };
    let fail = |error: &dyn std::fmt::Display| forge_error(&target.token, error);
    let (host, insecure) = match &target.api_base {
        Some(base_uri) => {
            let parsed = url::Url::parse(base_uri).map_err(|error| fail(&error))?;
            let host = parsed
                .host_str()
                .ok_or_else(|| PromoteError::UnknownForge(base_uri.clone()))?;
            let host = parsed
                .port()
                .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"));
            (host, parsed.scheme() == "http")
        }
        None => ("gitlab.com".to_owned(), false),
    };
    let mut builder = gitlab::GitlabBuilder::new(host, target.token.expose().to_owned());
    if insecure {
        builder.insecure();
    }
    let client = builder.build_async().await.map_err(|error| fail(&error))?;
    let listing = merge_requests::MergeRequests::builder()
        .project(target.project.as_str())
        .source_branch(head)
        .state(MergeRequestState::Opened)
        .build()
        .map_err(|error| fail(&error))?;
    let found: Vec<MergeRequestInfo> = paged(listing, Pagination::Limit(1))
        .query_async(&client)
        .await
        .map_err(|error| fail(&error))?;
    let (request, existing) = match found.into_iter().next() {
        Some(request) => (request, true),
        None => {
            let create = merge_requests::CreateMergeRequest::builder()
                .project(target.project.as_str())
                .source_branch(head)
                .target_branch(base)
                .title(title)
                .description(body)
                .build()
                .map_err(|error| fail(&error))?;
            let made: MergeRequestInfo = create
                .query_async(&client)
                .await
                .map_err(|error| fail(&error))?;
            (made, false)
        }
    };
    Ok(Published {
        forge: ForgeKind::GitLab,
        url: request.web_url,
        number: request.iid,
        existing,
    })
}
