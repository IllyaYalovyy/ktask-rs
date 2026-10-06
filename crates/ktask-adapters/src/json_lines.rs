//! The one reading of a JSON-lines stream: every line that is JSON, in order.

use ktask_core::Output;

pub(crate) fn events(output: &Output) -> Vec<serde_json::Value> {
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::events;
    use ktask_core::{Exit, Output};

    #[test]
    fn lines_that_are_not_json_are_skipped_and_the_rest_keep_their_order() {
        let output = Output {
            stdout: b"{\"n\":1}\nnot json\n\n{\"n\":2}\n[3]".to_vec(),
            stderr: Vec::new(),
            exit: Exit::Code(0),
        };
        let values: Vec<String> = events(&output).iter().map(ToString::to_string).collect();
        assert_eq!(values, ["{\"n\":1}", "{\"n\":2}", "[3]"]);
    }
}
