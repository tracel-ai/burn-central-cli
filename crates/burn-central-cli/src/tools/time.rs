use std::time::Duration;

pub fn format_elapsed_time(elapsed: Duration) -> String {
    let total_seconds = elapsed.as_secs();
    let millis = elapsed.subsec_millis();

    if total_seconds >= 3600 {
        let hours = total_seconds / 3600;
        let minutes = (total_seconds % 3600) / 60;
        let seconds = total_seconds % 60;
        format!("{}h {}m {}s", hours, minutes, seconds)
    } else if total_seconds >= 60 {
        let minutes = total_seconds / 60;
        let seconds = total_seconds % 60;
        format!("{}m {}s", minutes, seconds)
    } else if total_seconds > 0 {
        format!("{}s", total_seconds)
    } else {
        format!("{}ms", millis)
    }
}
