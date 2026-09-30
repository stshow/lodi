use std::cmp::Ordering;

use lodi::arch::version::PacmanVersion;

const CASES: &str = include_str!("fixtures/arch/version-cases.tsv");

fn check_table(table: &str) -> Result<(), String> {
    for (number, line) in table.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let columns: Vec<&str> = line.split('\t').collect();
        let [left, expected, right] = columns.as_slice() else {
            return Err(format!("case {} is not a three-column row", number + 1));
        };
        let left = PacmanVersion::parse(left)?;
        let right = PacmanVersion::parse(right)?;
        let actual = match left.cmp(&right) {
            Ordering::Less => "-1",
            Ordering::Equal => "0",
            Ordering::Greater => "1",
        };
        if actual != *expected {
            return Err(format!(
                "case {}: {} compared with {} produced {}, expected {}",
                number + 1,
                left.as_str(),
                right.as_str(),
                actual,
                expected
            ));
        }
        assert_eq!(right.cmp(&left), left.cmp(&right).reverse());
    }
    Ok(())
}

#[test]
fn committed_case_table_is_the_contract() {
    let rows = CASES
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .count();
    assert!(rows >= 60, "the contract has only {rows} cases");
    check_table(CASES).unwrap();
}

#[test]
fn a_deliberately_wrong_expected_row_fails_the_table() {
    let wrong = CASES.replacen("1.0\t0\t1.0", "1.0\t1\t1.0", 1);
    let error = check_table(&wrong).unwrap_err();
    assert!(error.contains("expected 1"), "{error}");
}

#[test]
fn invalid_versions_name_the_input() {
    for text in ["", "x:1.0", "1::2", "1.0-", "1.0-1-2", "1/2", "1 2"] {
        let error = PacmanVersion::parse(text).unwrap_err();
        assert!(error.contains(&format!("`{text}`")), "{error}");
    }
}
