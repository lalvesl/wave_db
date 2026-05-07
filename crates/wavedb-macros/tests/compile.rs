#[test]
fn ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/ok_*.rs");
    t.compile_fail("tests/ui/err_*.rs");
}
