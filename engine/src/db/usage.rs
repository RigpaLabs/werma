use anyhow::Result;
use rusqlite::params;

use crate::models::DailyUsage;

impl super::Db {
    pub fn increment_usage(&self, model: &str) -> Result<()> {
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        let column = match model {
            "opus" => "opus_calls",
            "sonnet" => "sonnet_calls",
            "haiku" => "haiku_calls",
            _ => anyhow::bail!("unknown model for usage tracking: {model}"),
        };

        let sql = format!(
            "INSERT INTO daily_usage (date, {column})
             VALUES (?1, 1)
             ON CONFLICT(date) DO UPDATE SET {column} = {column} + 1"
        );
        self.conn.execute(&sql, params![today])?;
        Ok(())
    }

    pub fn daily_usage(&self, date: &str) -> Result<DailyUsage> {
        let result = self.conn.query_row(
            "SELECT date, opus_calls, sonnet_calls, haiku_calls
             FROM daily_usage WHERE date = ?1",
            params![date],
            |row| {
                Ok(DailyUsage {
                    date: row.get(0)?,
                    opus_calls: row.get(1)?,
                    sonnet_calls: row.get(2)?,
                    haiku_calls: row.get(3)?,
                })
            },
        );

        match result {
            Ok(usage) => Ok(usage),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(DailyUsage {
                date: date.to_string(),
                opus_calls: 0,
                sonnet_calls: 0,
                haiku_calls: 0,
            }),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Db;

    #[test]
    fn daily_usage() {
        let db = Db::open_in_memory().unwrap();

        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        db.increment_usage("opus").unwrap();
        db.increment_usage("opus").unwrap();
        db.increment_usage("sonnet").unwrap();

        let usage = db.daily_usage(&today).unwrap();
        assert_eq!(usage.opus_calls, 2);
        assert_eq!(usage.sonnet_calls, 1);
        assert_eq!(usage.haiku_calls, 0);
    }

    #[test]
    fn daily_usage_no_data() {
        let db = Db::open_in_memory().unwrap();
        let usage = db.daily_usage("2020-01-01").unwrap();
        assert_eq!(usage.opus_calls, 0);
        assert_eq!(usage.sonnet_calls, 0);
    }

    #[test]
    fn increment_usage_haiku() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        db.increment_usage("haiku").unwrap();
        db.increment_usage("haiku").unwrap();
        db.increment_usage("haiku").unwrap();

        let usage = db.daily_usage(&today).unwrap();
        assert_eq!(usage.haiku_calls, 3);
        assert_eq!(usage.opus_calls, 0);
        assert_eq!(usage.sonnet_calls, 0);
    }

    #[test]
    fn increment_usage_unknown_model_errors() {
        let db = Db::open_in_memory().unwrap();
        let result = db.increment_usage("gpt-4");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown model"));
    }

    #[test]
    fn increment_usage_all_models() {
        let db = Db::open_in_memory().unwrap();
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();

        db.increment_usage("opus").unwrap();
        db.increment_usage("sonnet").unwrap();
        db.increment_usage("haiku").unwrap();

        let usage = db.daily_usage(&today).unwrap();
        assert_eq!(usage.opus_calls, 1);
        assert_eq!(usage.sonnet_calls, 1);
        assert_eq!(usage.haiku_calls, 1);
    }
}
