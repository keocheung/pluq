use std::path::PathBuf;

pub(crate) async fn run_bug_report() -> Result<(), Box<dyn std::error::Error>> {
    let mut report = String::new();

    // 1. pluq build
    match get_pluq_build() {
        Ok(s) => {
            report.push_str("pluq build:\n");
            report.push_str(&crate::redact::scrub_report(&s));
            report.push_str("\n\n");
        }
        Err(_) => {
            report.push_str("pluq build: unavailable\n\n");
        }
    }

    // 2. system
    match get_system() {
        Ok(s) => {
            report.push_str("system:\n");
            report.push_str(&crate::redact::scrub_report(&s));
            report.push_str("\n\n");
        }
        Err(_) => {
            report.push_str("system: unavailable\n\n");
        }
    }

    // 3. backends
    let backends_str = get_backends();
    report.push_str(&crate::redact::scrub_report(&backends_str));
    report.push_str("\n\n");

    // 4. environment
    match get_environment() {
        Ok(s) => {
            report.push_str(&crate::redact::scrub_report(&s));
            report.push_str("\n\n");
        }
        Err(_) => {
            report.push_str("environment: unavailable\n\n");
        }
    }

    // 5. terminal
    match get_terminal() {
        Ok(s) => {
            report.push_str("terminal:\n");
            report.push_str(&crate::redact::scrub_report(&s));
            report.push_str("\n\n");
        }
        Err(_) => {
            report.push_str("terminal: unavailable\n\n");
        }
    }

    // 6. Recent log tail
    match get_log_tail() {
        Ok(s) => {
            report.push_str(&s);
            report.push_str("\n\n");
        }
        Err(_) => {
            report.push_str("logs: unavailable\n\n");
        }
    }

    // 7. Last crash report
    match get_last_crash_report() {
        Ok(s) => {
            report.push_str(&s);
            report.push_str("\n\n");
        }
        Err(_) => {
            report.push_str("crash report: unavailable\n\n");
        }
    }

    // Write the report to pluq-bugreport-<unix_ts>.txt in the current working directory
    let unix_ts = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let filename = format!("pluq-bugreport-{}.txt", unix_ts);

    std::fs::write(&filename, report)?;

    // Resolve absolute path via canonicalize, falling back to relative if it fails
    let abs_path = std::fs::canonicalize(&filename).unwrap_or_else(|_| PathBuf::from(&filename));

    eprintln!("  Bug report written to: {}", abs_path.display());
    eprintln!("  This file was written locally. Nothing is sent anywhere.");
    eprintln!("  Review it, then attach it to a new issue if you choose:");
    eprintln!("    {}", crate::ISSUES_NEW_URL);

    Ok(())
}

fn get_pluq_build() -> Result<String, Box<dyn std::error::Error>> {
    let b = crate::build_info::build_info();
    let ts = chrono::DateTime::from_timestamp(b.timestamp, 0)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown".to_string());
    Ok(format!(
        "version: {}\ngit_sha: {}\ntarget: {}\ntimestamp: {}",
        b.version, b.git_sha, b.target, ts
    ))
}

fn get_system() -> Result<String, Box<dyn std::error::Error>> {
    let os = os_info::get().to_string();
    let arch = std::env::consts::ARCH;
    Ok(format!("os: {}\narch: {}", os, arch))
}

fn get_backends() -> String {
    match archive_auth::TokenStore::load_default() {
        Ok(store) => {
            let mut list = vec!["local".to_string()];
            if store.get("gdrive").is_some() {
                list.push("gdrive".to_string());
            }
            if store.get("dropbox").is_some() {
                list.push("dropbox".to_string());
            }
            format!("backends: {}", list.join(", "))
        }
        Err(_) => "backends: token store unavailable".to_string(),
    }
}

fn get_environment() -> Result<String, Box<dyn std::error::Error>> {
    let mut keys: Vec<String> = std::env::vars()
        .map(|(k, _)| k)
        .filter(|k| k.starts_with("PLUQ_"))
        .collect();
    keys.sort();

    let rust_log = std::env::var("RUST_LOG").unwrap_or_else(|_| "unset".to_string());
    let rust_backtrace = std::env::var("RUST_BACKTRACE").unwrap_or_else(|_| "unset".to_string());

    let pluq_str = if keys.is_empty() {
        "PLUQ_*: none".to_string()
    } else {
        format!("PLUQ_*:\n{}", keys.join("\n"))
    };

    Ok(format!(
        "environment:\nRUST_LOG={}\nRUST_BACKTRACE={}\n{}",
        rust_log, rust_backtrace, pluq_str
    ))
}

fn get_terminal() -> Result<String, Box<dyn std::error::Error>> {
    let term = std::env::var("TERM").unwrap_or_else(|_| "unset".to_string());
    let size = crossterm::terminal::size()
        .map(|(cols, rows)| format!("{} x {}", cols, rows))
        .unwrap_or_else(|_| "unknown".to_string());
    Ok(format!("TERM: {}\nsize: {}", term, size))
}

fn get_log_tail() -> Result<String, Box<dyn std::error::Error>> {
    let log_dir = match crate::log_path() {
        Some(dir) => dir,
        None => return Ok("logs: none found".to_string()),
    };
    if !log_dir.exists() {
        return Ok("logs: none found".to_string());
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(log_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && let Some(file_name) = path.file_name().and_then(|n| n.to_str())
            && file_name.starts_with("pluq.log")
            && let Ok(metadata) = entry.metadata()
        {
            files.push((path, metadata));
        }
    }

    // Sort newest first (by modification time)
    files.sort_by(|a, b| {
        let a_time = a.1.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let b_time = b.1.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        b_time.cmp(&a_time)
    });

    let newest = match files.first() {
        Some((path, _)) => path,
        None => return Ok("logs: none found".to_string()),
    };

    let content = std::fs::read_to_string(newest)?;
    let lines: Vec<&str> = content.lines().collect();
    let tail_lines = if lines.len() > 200 {
        &lines[lines.len() - 200..]
    } else {
        &lines[..]
    };

    let scrubbed_lines: Vec<String> = tail_lines
        .iter()
        .map(|line| crate::redact::scrub_report(line))
        .collect();

    Ok(format!("logs:\n{}", scrubbed_lines.join("\n")))
}

fn get_last_crash_report() -> Result<String, Box<dyn std::error::Error>> {
    let temp_dir = std::env::temp_dir();
    if !temp_dir.exists() {
        return Ok("crash report: none found".to_string());
    }

    let mut files = Vec::new();
    for entry in std::fs::read_dir(temp_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && let Some(file_name) = path.file_name().and_then(|n| n.to_str())
            && file_name.starts_with("report-")
            && file_name.ends_with(".toml")
            && let Ok(metadata) = entry.metadata()
        {
            files.push((path, metadata));
        }
    }

    // Sort newest first
    files.sort_by(|a, b| {
        let a_time = a.1.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        let b_time = b.1.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
        b_time.cmp(&a_time)
    });

    let newest = match files.first() {
        Some((path, _)) => path,
        None => return Ok("crash report: none found".to_string()),
    };

    let content = std::fs::read_to_string(newest)?;
    let scrubbed = crate::redact::scrub_report(&content);
    Ok(format!("crash report:\n{}", scrubbed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_get_pluq_build() {
        let res = get_pluq_build().unwrap();
        assert!(res.contains("version:"));
        assert!(res.contains("git_sha:"));
    }

    #[test]
    fn test_get_system() {
        let res = get_system().unwrap();
        assert!(res.contains("os:"));
        assert!(res.contains("arch:"));
    }

    #[test]
    fn test_get_backends() {
        let res = get_backends();
        assert!(res.contains("backends:") || res.contains("token store unavailable"));
    }

    #[test]
    fn test_get_environment() {
        let res = get_environment().unwrap();
        assert!(res.contains("environment:"));
    }

    #[test]
    fn test_get_terminal() {
        let res = get_terminal().unwrap();
        assert!(res.contains("TERM:"));
        assert!(res.contains("size:"));
    }

    #[test]
    fn test_get_log_tail() {
        let res = get_log_tail().unwrap();
        assert!(res.contains("logs:"));
    }

    #[test]
    fn test_get_last_crash_report() {
        let res = get_last_crash_report().unwrap();
        assert!(res.contains("crash report:"));
    }
}
