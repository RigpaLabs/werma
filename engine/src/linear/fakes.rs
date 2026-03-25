use anyhow::{Result, bail};
use serde_json::Value;
use std::cell::RefCell;
use std::collections::HashMap;

use super::api::LinearApi;

/// Fake LinearApi that records calls and returns pre-configured responses.
/// Use `set_issues_for_status`/`set_issues_for_label` to configure per-key responses.
pub struct FakeLinearApi {
    pub issues_by_status: RefCell<HashMap<String, Vec<Value>>>,
    pub issues_by_label: RefCell<HashMap<String, Vec<Value>>>,
    pub issue_details: RefCell<Option<(String, String, String, String, Vec<String>)>>,
    pub move_calls: RefCell<Vec<(String, String)>>,
    pub comment_calls: RefCell<Vec<(String, String)>>,
    pub attach_calls: RefCell<Vec<(String, String, String)>>,
    pub estimate_calls: RefCell<Vec<(String, i32)>>,
    pub remove_label_calls: RefCell<Vec<(String, String)>>,
    pub add_label_calls: RefCell<Vec<(String, String)>>,
}

impl FakeLinearApi {
    pub fn new() -> Self {
        Self {
            issues_by_status: RefCell::new(HashMap::new()),
            issues_by_label: RefCell::new(HashMap::new()),
            issue_details: RefCell::new(None),
            move_calls: RefCell::new(vec![]),
            comment_calls: RefCell::new(vec![]),
            attach_calls: RefCell::new(vec![]),
            estimate_calls: RefCell::new(vec![]),
            remove_label_calls: RefCell::new(vec![]),
            add_label_calls: RefCell::new(vec![]),
        }
    }

    /// Set issues returned for a specific status name.
    pub fn set_issues_for_status(&self, status: &str, issues: Vec<Value>) {
        self.issues_by_status
            .borrow_mut()
            .insert(status.to_string(), issues);
    }

    /// Set issues returned for a specific label name.
    pub fn set_issues_for_label(&self, label: &str, issues: Vec<Value>) {
        self.issues_by_label
            .borrow_mut()
            .insert(label.to_string(), issues);
    }
}

impl LinearApi for FakeLinearApi {
    fn get_issues_by_status(&self, status_name: &str) -> Result<Vec<Value>> {
        Ok(self
            .issues_by_status
            .borrow()
            .get(status_name)
            .cloned()
            .unwrap_or_default())
    }

    fn get_issues_by_label(&self, label_name: &str) -> Result<Vec<Value>> {
        Ok(self
            .issues_by_label
            .borrow()
            .get(label_name)
            .cloned()
            .unwrap_or_default())
    }

    fn get_issue(&self, _issue_id: &str) -> Result<(String, String)> {
        if let Some(ref d) = *self.issue_details.borrow() {
            Ok((d.2.clone(), d.3.clone()))
        } else {
            Ok((String::new(), String::new()))
        }
    }

    fn get_issue_by_identifier(
        &self,
        _identifier: &str,
    ) -> Result<(String, String, String, String, Vec<String>)> {
        if let Some(ref d) = *self.issue_details.borrow() {
            Ok(d.clone())
        } else {
            bail!("issue not found")
        }
    }

    fn move_issue_by_name(&self, issue_id: &str, status_name: &str) -> Result<()> {
        self.move_calls
            .borrow_mut()
            .push((issue_id.to_string(), status_name.to_string()));
        Ok(())
    }

    fn comment(&self, issue_id: &str, body: &str) -> Result<()> {
        self.comment_calls
            .borrow_mut()
            .push((issue_id.to_string(), body.to_string()));
        Ok(())
    }

    fn attach_url(&self, issue_id: &str, url: &str, title: &str) -> Result<()> {
        self.attach_calls.borrow_mut().push((
            issue_id.to_string(),
            url.to_string(),
            title.to_string(),
        ));
        Ok(())
    }

    fn update_estimate(&self, issue_id: &str, estimate: i32) -> Result<()> {
        self.estimate_calls
            .borrow_mut()
            .push((issue_id.to_string(), estimate));
        Ok(())
    }

    fn remove_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        self.remove_label_calls
            .borrow_mut()
            .push((issue_id.to_string(), label_name.to_string()));
        Ok(())
    }

    fn add_label(&self, issue_id: &str, label_name: &str) -> Result<()> {
        self.add_label_calls
            .borrow_mut()
            .push((issue_id.to_string(), label_name.to_string()));
        Ok(())
    }

    fn get_issue_status(&self, _issue_id: &str) -> Result<String> {
        Ok(String::new())
    }

    fn get_issue_state_and_team(&self, _issue_id: &str) -> Result<(String, String)> {
        Ok(("started".to_string(), "RIG".to_string()))
    }

    fn list_comments(
        &self,
        _issue_id: &str,
        _after_iso: Option<&str>,
    ) -> Result<Vec<(String, String, String)>> {
        Ok(vec![])
    }
}
