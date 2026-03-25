use std::cell::RefCell;
use std::collections::HashMap;

use anyhow::Result;

use crate::models::{Schedule, Status, Task};

use super::schedule::ScheduleRepository;
use super::task::TaskRepository;

/// In-memory fake implementing TaskRepository for testing consumers
/// without touching SQLite.
pub struct FakeTaskRepo {
    tasks: RefCell<HashMap<String, Task>>,
    next_id_counter: RefCell<u32>,
}

impl FakeTaskRepo {
    pub fn new() -> Self {
        Self {
            tasks: RefCell::new(HashMap::new()),
            next_id_counter: RefCell::new(1),
        }
    }
}

impl TaskRepository for FakeTaskRepo {
    fn next_task_id(&self) -> Result<String> {
        let n = *self.next_id_counter.borrow();
        *self.next_id_counter.borrow_mut() = n + 1;
        Ok(format!("20260325-{n:03}"))
    }

    fn insert_task(&self, task: &Task) -> Result<()> {
        self.tasks
            .borrow_mut()
            .insert(task.id.clone(), task.clone());
        Ok(())
    }

    fn task(&self, id: &str) -> Result<Option<Task>> {
        Ok(self.tasks.borrow().get(id).cloned())
    }

    fn list_tasks(&self, status: Option<Status>) -> Result<Vec<Task>> {
        let tasks = self.tasks.borrow();
        let iter = tasks.values();
        match status {
            Some(s) => Ok(iter.filter(|t| t.status == s).cloned().collect()),
            None => Ok(iter.cloned().collect()),
        }
    }

    fn list_recent_tasks(&self, status: Status, limit: usize) -> Result<Vec<Task>> {
        let tasks = self.tasks.borrow();
        let mut matching: Vec<_> = tasks.values().filter(|t| t.status == status).collect();
        matching.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
        Ok(matching.into_iter().take(limit).cloned().collect())
    }

    fn list_all_tasks_by_finished(&self, status: Status) -> Result<Vec<Task>> {
        let tasks = self.tasks.borrow();
        let mut matching: Vec<_> = tasks.values().filter(|t| t.status == status).collect();
        matching.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
        Ok(matching.into_iter().cloned().collect())
    }

    fn list_recent_terminal_tasks(&self, limit: usize) -> Result<Vec<Task>> {
        let tasks = self.tasks.borrow();
        let mut matching: Vec<_> = tasks
            .values()
            .filter(|t| {
                matches!(
                    t.status,
                    Status::Completed | Status::Failed | Status::Canceled
                )
            })
            .collect();
        matching.sort_by(|a, b| b.finished_at.cmp(&a.finished_at));
        Ok(matching.into_iter().take(limit).cloned().collect())
    }

    fn set_task_status(&self, id: &str, status: Status) -> Result<()> {
        if let Some(task) = self.tasks.borrow_mut().get_mut(id) {
            task.status = status;
        }
        Ok(())
    }

    fn find_next_pending(&self) -> Result<Option<Task>> {
        let tasks = self.tasks.borrow();
        Ok(tasks
            .values()
            .filter(|t| t.status == Status::Pending)
            .min_by_key(|t| t.priority)
            .cloned())
    }

    fn update_task_field(&self, id: &str, field: &str, value: &str) -> Result<()> {
        if let Some(task) = self.tasks.borrow_mut().get_mut(id) {
            match field {
                "session_id" => task.session_id = value.to_string(),
                "started_at" => task.started_at = Some(value.to_string()),
                "finished_at" => task.finished_at = Some(value.to_string()),
                "output_path" => task.output_path = value.to_string(),
                "pipeline_stage" => task.pipeline_stage = value.to_string(),
                "allowed_tools" => task.allowed_tools = value.to_string(),
                "model" => task.model = value.to_string(),
                "repo_hash" => task.repo_hash = value.to_string(),
                _ => {}
            }
        }
        Ok(())
    }
}

/// In-memory fake implementing ScheduleRepository.
pub struct FakeScheduleRepo {
    schedules: RefCell<HashMap<String, Schedule>>,
}

impl FakeScheduleRepo {
    pub fn new() -> Self {
        Self {
            schedules: RefCell::new(HashMap::new()),
        }
    }
}

impl ScheduleRepository for FakeScheduleRepo {
    fn insert_schedule(&self, sched: &Schedule) -> Result<()> {
        self.schedules
            .borrow_mut()
            .insert(sched.id.clone(), sched.clone());
        Ok(())
    }

    fn list_schedules(&self) -> Result<Vec<Schedule>> {
        let scheds = self.schedules.borrow();
        let mut result: Vec<_> = scheds.values().cloned().collect();
        result.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(result)
    }

    fn schedule(&self, id: &str) -> Result<Option<Schedule>> {
        Ok(self.schedules.borrow().get(id).cloned())
    }

    fn delete_schedule(&self, id: &str) -> Result<()> {
        self.schedules.borrow_mut().remove(id);
        Ok(())
    }

    fn set_schedule_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        if let Some(sched) = self.schedules.borrow_mut().get_mut(id) {
            sched.enabled = enabled;
        }
        Ok(())
    }

    fn set_schedule_last_enqueued(&self, id: &str, timestamp: &str) -> Result<()> {
        if let Some(sched) = self.schedules.borrow_mut().get_mut(id) {
            sched.last_enqueued = timestamp.to_string();
        }
        Ok(())
    }
}
