//! Background service that polls for Linear issues pending GitNexus impact analysis
//! and spawns Claude Code to analyze them.

use std::{path::PathBuf, time::Duration};

use tokio::time::{MissedTickBehavior, interval};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

/// Extracts all markdown image URLs from a piece of text.
fn extract_image_urls(text: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("![") {
        rest = &rest[start + 2..];
        // skip label
        let Some(close_bracket) = rest.find("](") else { continue };
        rest = &rest[close_bracket + 2..];
        let Some(close_paren) = rest.find(')') else { continue };
        let url = &rest[..close_paren];
        if url.starts_with("http") {
            urls.push(url.to_string());
        }
        rest = &rest[close_paren + 1..];
    }
    urls
}

/// Downloads image URLs using the Linear API key and saves them as temp files.
/// Returns a list of (temp_path, original_url) pairs for successfully downloaded images.
async fn download_images(
    urls: &[String],
    api_key: &str,
    tmp_dir: &std::path::Path,
) -> Vec<PathBuf> {
    let client = reqwest::Client::new();
    let mut paths = Vec::new();
    for (i, url) in urls.iter().enumerate() {
        let result = client
            .get(url)
            .header("Authorization", api_key)
            .send()
            .await;
        match result {
            Ok(resp) if resp.status().is_success() => {
                // Detect extension from content-type
                let ext = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .and_then(|ct| {
                        if ct.contains("png") {
                            Some("png")
                        } else if ct.contains("jpeg") || ct.contains("jpg") {
                            Some("jpg")
                        } else if ct.contains("gif") {
                            Some("gif")
                        } else if ct.contains("webp") {
                            Some("webp")
                        } else {
                            None
                        }
                    })
                    .unwrap_or("png");
                let path = tmp_dir.join(format!("linear_img_{i}.{ext}"));
                match resp.bytes().await {
                    Ok(bytes) => {
                        if tokio::fs::write(&path, &bytes).await.is_ok() {
                            paths.push(path);
                        }
                    }
                    Err(e) => warn!(?e, url, "Failed to read image bytes"),
                }
            }
            Ok(resp) => warn!(url, status = %resp.status(), "Failed to download image"),
            Err(e) => warn!(?e, url, "Failed to fetch image"),
        }
    }
    paths
}

use super::remote_client::RemoteClient;

const POLL_INTERVAL_SECS: u64 = 60;

pub struct LinearAnalysisService {
    remote_client: RemoteClient,
    repo_path: PathBuf,
}

impl LinearAnalysisService {
    /// Spawn the background service. Returns None if no remote client or repo path is configured.
    pub fn spawn(
        remote_client: Option<RemoteClient>,
        repo_path: Option<PathBuf>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        let remote_client = remote_client?;
        let repo_path = repo_path?;

        let service = Self {
            remote_client,
            repo_path,
        };

        Some(tokio::spawn(async move {
            service.start().await;
        }))
    }

    async fn start(&self) {
        info!(
            "Starting Linear GitNexus analysis service (repo: {}, interval: {}s)",
            self.repo_path.display(),
            POLL_INTERVAL_SECS
        );

        let mut ticker = interval(Duration::from_secs(POLL_INTERVAL_SECS));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        ticker.tick().await; // skip immediate first tick

        loop {
            ticker.tick().await;
            self.run_once().await;
        }
    }

    async fn run_once(&self) {
        let issues = match self.fetch_pending_issues().await {
            Ok(v) => v,
            Err(e) => {
                warn!(?e, "Failed to fetch pending Linear analysis issues");
                return;
            }
        };

        if issues.is_empty() {
            debug!("No Linear issues pending GitNexus analysis");
            return;
        }

        info!("{} Linear issue(s) pending GitNexus analysis", issues.len());

        for (issue_id, project_id, title, description, worktree_branch, comments, linear_api_key) in issues {
            self.analyze_issue(
                issue_id,
                project_id,
                &title,
                description.as_deref(),
                worktree_branch.as_deref(),
                &comments,
                linear_api_key.as_deref(),
            )
            .await;
        }
    }

    async fn fetch_pending_issues(
        &self,
    ) -> Result<Vec<(Uuid, Uuid, String, Option<String>, Option<String>, Vec<String>, Option<String>)>, String> {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct PendingIssue {
            issue_id: Uuid,
            project_id: Uuid,
            title: String,
            description: Option<String>,
            worktree_branch: Option<String>,
            #[serde(default)]
            comments: Vec<String>,
            linear_api_key: Option<String>,
        }

        let token = self
            .remote_client
            .access_token()
            .await
            .map_err(|e| e.to_string())?;

        let base = self.remote_client.base_url();
        let base = if base.ends_with('/') {
            base.to_string()
        } else {
            format!("{base}/")
        };
        let url = format!("{base}v1/linear/pending-analysis");

        let resp = reqwest::Client::new()
            .get(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }

        let issues: Vec<PendingIssue> = resp.json().await.map_err(|e| e.to_string())?;
        Ok(issues
            .into_iter()
            .map(|i| {
                (
                    i.issue_id,
                    i.project_id,
                    i.title,
                    i.description,
                    i.worktree_branch,
                    i.comments,
                    i.linear_api_key,
                )
            })
            .collect())
    }

    async fn analyze_issue(
        &self,
        issue_id: Uuid,
        project_id: Uuid,
        title: &str,
        description: Option<&str>,
        worktree_branch: Option<&str>,
        comments: &[String],
        linear_api_key: Option<&str>,
    ) {
        // Collect all image URLs from description and comments
        let mut image_urls: Vec<String> = Vec::new();
        if let Some(desc) = description {
            image_urls.extend(extract_image_urls(desc));
        }
        for comment in comments {
            image_urls.extend(extract_image_urls(comment));
        }

        // Download images to a temp directory if we have an API key
        let tmp_dir = std::env::temp_dir().join(format!("gitnexus_imgs_{issue_id}"));
        let image_paths = if !image_urls.is_empty() {
            if let Some(api_key) = linear_api_key {
                let _ = tokio::fs::create_dir_all(&tmp_dir).await;
                let paths = download_images(&image_urls, api_key, &tmp_dir).await;
                info!(%issue_id, count = paths.len(), "Downloaded Linear images for analysis");
                paths
            } else {
                vec![]
            }
        } else {
            vec![]
        };

        let prompt = build_prompt(title, description, worktree_branch, comments, &image_paths);

        info!(%issue_id, %title, "Running GitNexus analysis for Linear issue");

        // Inherit the current process environment so claude can find its auth
        // credentials (keychain access, HOME, PATH, etc.)
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::env::var("PATH").unwrap_or_default();

        let output = tokio::process::Command::new("claude")
            .arg("--dangerously-skip-permissions")
            .arg("--no-session-persistence")
            .arg("--no-chrome")
            .arg("--setting-sources")
            .arg("project,local")
            .arg("--mcp-config")
            .arg(gitnexus_mcp_config_arg())
            .arg("--output-format")
            .arg("text")
            .arg("--print")
            .arg(&prompt)
            .current_dir(&self.repo_path)
            .env("HOME", &home)
            .env("PATH", &path)
            .stdin(std::process::Stdio::null())
            .output()
            .await;

        // Clean up temp images
        if tmp_dir.exists() {
            let _ = tokio::fs::remove_dir_all(&tmp_dir).await;
        }

        match output {
            Err(e) => {
                error!(?e, %issue_id, "Failed to spawn claude for GitNexus analysis");
            }
            Ok(out) if !out.status.success() => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                error!(%issue_id, %stderr, "Claude exited with error during GitNexus analysis");
            }
            Ok(out) => {
                let analysis = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if analysis.is_empty() {
                    warn!(%issue_id, "GitNexus analysis returned empty output");
                    return;
                }

                let comment = format!("## GitNexus Impact Analysis\n\n{analysis}");

                if let Err(e) = self
                    .remote_client
                    .create_issue_comment(issue_id, &comment)
                    .await
                {
                    error!(?e, %issue_id, "Failed to post GitNexus analysis comment");
                    return;
                }

                if let Err(e) = self.mark_analyzed(issue_id).await {
                    error!(?e, %issue_id, "Failed to mark issue as analyzed");
                } else {
                    info!(%issue_id, "GitNexus analysis comment posted successfully");
                }

                // Auto-start workspace if impact score < 50 and branch label is set
                let parsed_score = parse_impact_score(&analysis);
                debug!(%issue_id, ?worktree_branch, ?parsed_score, "Auto-start check");
                if let Some(branch) = worktree_branch
                    && let Some(score) = parsed_score
                    && score < 50
                {
                    info!(%issue_id, score, branch, "Low impact score — auto-starting workspace");
                    match self
                        .auto_start_workspace(issue_id, project_id, title, description, comments, branch)
                        .await
                    {
                        Ok(()) => {}
                        Err(e) if e.contains("does not exist") || e.contains("BranchNotFound") => {
                            warn!(%issue_id, branch, "Branch not found on remote — skipping auto-start");
                            let comment = format!(
                                "⚠️ **GitNexus**: Could not auto-start workspace — branch `{branch}` does not exist on the remote. Push the branch first and re-trigger analysis."
                            );
                            if let Err(ce) = self
                                .remote_client
                                .create_issue_comment(issue_id, &comment)
                                .await
                            {
                                error!(?ce, %issue_id, "Failed to post branch-not-found comment");
                            }
                        }
                        Err(e) => {
                            error!(?e, %issue_id, "Failed to auto-start workspace");
                        }
                    }
                }
            }
        }
    }

    async fn auto_start_workspace(
        &self,
        issue_id: Uuid,
        project_id: Uuid,
        title: &str,
        description: Option<&str>,
        comments: &[String],
        branch: &str,
    ) -> Result<(), String> {
        // Read the local backend port from the port file
        let port = utils::port_file::read_port_file("vibe-kanban")
            .await
            .map_err(|e| format!("Failed to read port file: {e}"))?;

        let local_url = format!("http://127.0.0.1:{port}");

        // Get all repos from the local backend
        #[derive(serde::Deserialize)]
        struct RepoListResponse {
            data: Vec<RepoItem>,
        }
        #[derive(serde::Deserialize)]
        struct RepoItem {
            id: uuid::Uuid,
            path: std::path::PathBuf,
        }

        let repos_resp = reqwest::Client::new()
            .get(format!("{local_url}/api/repos"))
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !repos_resp.status().is_success() {
            return Err(format!(
                "Failed to list repos: HTTP {}",
                repos_resp.status()
            ));
        }

        let repos_body = repos_resp.text().await.map_err(|e| e.to_string())?;
        let repo_list: RepoListResponse =
            serde_json::from_str(&repos_body).map_err(|e| format!("{e}: {repos_body}"))?;

        // Prefer the repo whose path matches INKU_REPO_PATH, fall back to first
        let repo_id = repo_list
            .data
            .iter()
            .find(|r| r.path == self.repo_path)
            .or_else(|| repo_list.data.first())
            .ok_or("No repos configured")?
            .id;

        #[derive(serde::Serialize)]
        struct AutoStartRequest {
            name: Option<String>,
            repos: Vec<RepoInput>,
            executor_config: serde_json::Value,
            prompt: String,
        }
        #[derive(serde::Serialize)]
        struct RepoInput {
            repo_id: uuid::Uuid,
            target_branch: String,
        }

        let prompt = build_workspace_prompt(title, description, comments);
        let payload = AutoStartRequest {
            name: Some(title.to_string()),
            repos: vec![RepoInput {
                repo_id,
                target_branch: branch.to_string(),
            }],
            executor_config: serde_json::json!({"executor": "CLAUDE_CODE"}),
            prompt,
        };

        let resp = reqwest::Client::new()
            .post(format!("{local_url}/api/workspaces/start"))
            .json(&payload)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(format!("Failed to start workspace: HTTP {status}: {body}"));
        }

        // Parse the local workspace ID from the response so we can notify the remote server
        #[derive(serde::Deserialize)]
        struct StartResponse {
            data: StartResponseData,
        }
        #[derive(serde::Deserialize)]
        struct StartResponseData {
            workspace: WorkspaceData,
        }
        #[derive(serde::Deserialize)]
        struct WorkspaceData {
            id: Uuid,
        }

        let body = resp.text().await.map_err(|e| e.to_string())?;
        let local_workspace_id = serde_json::from_str::<StartResponse>(&body)
            .map(|r| r.data.workspace.id)
            .map_err(|e| format!("Failed to parse workspace start response: {e}: {body}"))?;

        info!(%issue_id, branch, "Workspace auto-started successfully");

        // Notify the remote server so it can sync the issue status to "In Progress"
        use api_types::CreateWorkspaceRequest;
        if let Err(e) = self
            .remote_client
            .create_workspace(CreateWorkspaceRequest {
                project_id,
                local_workspace_id,
                issue_id,
                name: Some(title.to_string()),
                archived: None,
                files_changed: None,
                lines_added: None,
                lines_removed: None,
            })
            .await
        {
            warn!(%issue_id, ?e, "Failed to notify remote server of workspace creation (status may not update)");
        }

        Ok(())
    }

    async fn mark_analyzed(&self, issue_id: Uuid) -> Result<(), String> {
        let token = self
            .remote_client
            .access_token()
            .await
            .map_err(|e| e.to_string())?;

        let base = self.remote_client.base_url();
        let base = if base.ends_with('/') {
            base.to_string()
        } else {
            format!("{base}/")
        };
        let url = format!("{base}v1/linear/issues/{issue_id}/mark-analyzed");

        let resp = reqwest::Client::new()
            .post(&url)
            .bearer_auth(&token)
            .send()
            .await
            .map_err(|e| e.to_string())?;

        if !resp.status().is_success() {
            return Err(format!("HTTP {}", resp.status()));
        }
        Ok(())
    }
}

fn parse_impact_score(analysis: &str) -> Option<u32> {
    // Looks for "**Impact Score: XX/100**" anywhere in the text
    for line in analysis.lines() {
        let line = line.trim();
        if let Some(rest) = line.trim_matches('*').trim().strip_prefix("Impact Score:") {
            let rest = rest.trim().trim_matches('*').trim();
            if let Some(score_str) = rest.split('/').next()
                && let Ok(score) = score_str.trim().parse::<u32>()
            {
                return Some(score);
            }
        }
    }
    None
}

fn build_prompt(
    title: &str,
    description: Option<&str>,
    branch: Option<&str>,
    comments: &[String],
    image_paths: &[PathBuf],
) -> String {
    let desc_section = description
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!("\n\nDescription:\n{d}"))
        .unwrap_or_default();

    let comments_section = if comments.is_empty() {
        String::new()
    } else {
        let joined = comments
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\nComments:\n{joined}")
    };

    let branch_instruction = branch
        .map(|b| format!("\n\nThis issue is associated with branch `{b}`. Before analyzing, run `git checkout {b}` (or `git fetch origin {b} && git checkout {b}` if it doesn't exist locally) to ensure you are analyzing the correct code."))
        .unwrap_or_default();

    let images_section = if image_paths.is_empty() {
        String::new()
    } else {
        let paths = image_paths
            .iter()
            .filter_map(|p| p.to_str())
            .map(|p| format!("- {p}"))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\nThe issue contains screenshots/images that have been downloaded for you. Please read and consider them as part of your analysis:\n{paths}")
    };

    format!(
        "You are analyzing a software issue to determine its impact on the codebase.\n\
        \n\
        Issue title: {title}{desc_section}{comments_section}{branch_instruction}{images_section}\n\
        \n\
        Using the GitNexus MCP tools available to you:\n\
        1. Read the repo context first\n\
        2. Use the `query` or `impact` tool to analyze what parts of the codebase are affected by this issue\n\
        3. Provide a concise impact analysis including:\n\
           - Affected components/files\n\
           - Blast radius (what could break)\n\
           - Suggested approach\n\
           - Impact score (Low/Medium/High/Critical)\n\
        \n\
        Keep the response concise and actionable. Always end with a line: **Impact Score: XX/100**\n\
        \n\
        If you need clarification about the issue or the expected behavior, you can post a question in the Slack channel C0ALP6LUF1R using the Slack MCP tool before completing your analysis."
    )
}

fn build_workspace_prompt(title: &str, description: Option<&str>, comments: &[String]) -> String {
    let desc_section = description
        .filter(|d| !d.trim().is_empty())
        .map(|d| format!("\n\nDescription:\n{d}"))
        .unwrap_or_default();

    let comments_section = if comments.is_empty() {
        String::new()
    } else {
        let joined = comments
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{}. {c}", i + 1))
            .collect::<Vec<_>>()
            .join("\n");
        format!("\n\nComments:\n{joined}")
    };

    format!("Work on: {title}{desc_section}{comments_section}\n\nIMPORTANT: Do not start, run, or restart any services, servers, or Docker containers. Only read and modify code files.")
}

fn gitnexus_mcp_config_arg() -> String {
    // Inline JSON config for GitNexus + Slack MCP servers
    r#"{"mcpServers":{"gitnexus":{"command":"npx","args":["-y","gitnexus@latest","mcp"]},"slack":{"command":"npx","args":["-y","@modelcontextprotocol/server-slack"],"env":{"SLACK_BOT_TOKEN":"slack key","SLACK_TEAM_ID":"slack team id"}}}}"#
        .to_string()
}
