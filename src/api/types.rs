//! Serde models for the Asana objects the CLI touches.
//!
//! Every field except `gid` is optional so that partial objects (whatever
//! `opt_fields` we asked for) deserialize without complaint.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

/// Fields requested for `/users/me`.
pub const USER_FIELDS: &str = "name,email,workspaces.name";

/// Fields requested when listing or viewing projects.
pub const PROJECT_FIELDS: &str = "name,archived,color,due_on,modified_at,owner.name,current_status.title,current_status.color,team.name,permalink_url,notes,members,created_at,public,completed,start_on";

/// Fields requested for task lists and views.
pub const TASK_FIELDS: &str = "name,notes,completed,completed_at,assignee.name,assignee.email,due_on,due_at,start_on,created_at,modified_at,projects.name,memberships.section.name,memberships.project.name,tags.name,parent.name,num_subtasks,permalink_url,created_by.name,followers.name,num_likes,custom_fields.name,custom_fields.display_value,workspace.name";

/// Fields requested for a single task view (adds `html_notes` for rendering).
pub const TASK_VIEW_FIELDS: &str = "name,notes,html_notes,completed,completed_at,assignee.name,assignee.email,due_on,due_at,start_on,created_at,modified_at,projects.name,memberships.section.name,memberships.project.name,tags.name,parent.name,num_subtasks,permalink_url,created_by.name,followers.name,num_likes,custom_fields.name,custom_fields.display_value,workspace.name";

/// Fields requested for task stories (comments and system events).
pub const STORY_FIELDS: &str =
    "created_at,created_by.name,text,type,resource_subtype,is_pinned,num_likes,html_text";

/// Fields requested for subtasks.
pub const SUBTASK_FIELDS: &str = "name,completed,assignee.name,due_on";

/// Fields requested for `/projects/{gid}/task_counts`.
pub const TASK_COUNT_FIELDS: &str = "num_tasks,num_incomplete_tasks,num_completed_tasks";

/// Any compact `{ gid, name }` reference (owner, team, tag, parent, ...).
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Named {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

impl Named {
    /// The display name, or the gid when the API did not send one.
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.gid)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct User {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
    pub email: Option<String>,
    #[serde(default)]
    pub workspaces: Vec<Workspace>,
}

impl User {
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.gid)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
}

impl Workspace {
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.gid)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectStatus {
    pub title: Option<String>,
    pub color: Option<String>,
    pub text: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Project {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
    pub archived: Option<bool>,
    pub color: Option<String>,
    pub due_on: Option<NaiveDate>,
    pub start_on: Option<NaiveDate>,
    pub modified_at: Option<DateTime<Utc>>,
    pub created_at: Option<DateTime<Utc>>,
    pub owner: Option<Named>,
    pub current_status: Option<ProjectStatus>,
    pub team: Option<Named>,
    pub permalink_url: Option<String>,
    pub notes: Option<String>,
    pub html_notes: Option<String>,
    #[serde(default)]
    pub members: Vec<Named>,
    pub public: Option<bool>,
    pub completed: Option<bool>,
    pub workspace: Option<Workspace>,
}

impl Project {
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.gid)
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Membership {
    pub project: Option<Named>,
    pub section: Option<Named>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CustomField {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
    pub display_value: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Task {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
    pub notes: Option<String>,
    pub html_notes: Option<String>,
    pub completed: Option<bool>,
    pub completed_at: Option<DateTime<Utc>>,
    pub assignee: Option<Named>,
    pub due_on: Option<NaiveDate>,
    pub due_at: Option<DateTime<Utc>>,
    pub start_on: Option<NaiveDate>,
    pub created_at: Option<DateTime<Utc>>,
    pub modified_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub projects: Vec<Named>,
    #[serde(default)]
    pub memberships: Vec<Membership>,
    #[serde(default)]
    pub tags: Vec<Named>,
    pub parent: Option<Named>,
    pub num_subtasks: Option<u64>,
    pub permalink_url: Option<String>,
    pub created_by: Option<Named>,
    #[serde(default)]
    pub followers: Vec<Named>,
    pub num_likes: Option<u64>,
    #[serde(default)]
    pub custom_fields: Vec<CustomField>,
    pub workspace: Option<Workspace>,
}

impl Task {
    pub fn display(&self) -> &str {
        self.name.as_deref().unwrap_or(&self.gid)
    }

    pub fn is_completed(&self) -> bool {
        self.completed.unwrap_or(false)
    }

    /// Project names, each with its section in parentheses when known.
    pub fn project_labels(&self) -> Vec<String> {
        if !self.memberships.is_empty() {
            return self
                .memberships
                .iter()
                .filter_map(|m| {
                    let project = m.project.as_ref()?;
                    let section = m
                        .section
                        .as_ref()
                        .and_then(|s| s.name.as_deref())
                        .filter(|s| !s.is_empty() && *s != "Untitled section");
                    Some(match section {
                        Some(section) => format!("{} ({section})", project.display()),
                        None => project.display().to_string(),
                    })
                })
                .collect();
        }
        self.projects
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Story {
    #[serde(default)]
    pub gid: String,
    pub created_at: Option<DateTime<Utc>>,
    pub created_by: Option<Named>,
    pub text: Option<String>,
    pub html_text: Option<String>,
    #[serde(rename = "type")]
    pub story_type: Option<String>,
    pub resource_subtype: Option<String>,
    pub is_pinned: Option<bool>,
    pub num_likes: Option<u64>,
}

impl Story {
    pub fn is_comment(&self) -> bool {
        self.story_type.as_deref() == Some("comment")
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Section {
    #[serde(default)]
    pub gid: String,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TaskCounts {
    pub num_tasks: Option<u64>,
    pub num_incomplete_tasks: Option<u64>,
    pub num_completed_tasks: Option<u64>,
}
