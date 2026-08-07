//! pg_regress schedule files: `test: name [name ...]` lines define groups
//! that upstream may run in parallel; this runner executes everything
//! serially in file order (pg_regress supports that too).

pub fn parse_schedule(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| line.trim().strip_prefix("test:"))
        .flat_map(|names| names.split_whitespace().map(String::from))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flattens_groups_in_file_order() {
        let schedule = "\
# comment
test: test_setup

test: boolean char
test: int4
";
        assert_eq!(
            parse_schedule(schedule),
            ["test_setup", "boolean", "char", "int4"]
        );
    }
}
