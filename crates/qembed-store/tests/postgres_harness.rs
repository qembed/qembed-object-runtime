mod common;

#[test]
fn postgres_harness_connects_to_supported_server() {
    let mut client = common::connect();

    let ready: i32 = client
        .query_one("SELECT 1", &[])
        .expect("the test database should execute a simple query")
        .get(0);
    assert_eq!(ready, 1);

    let version: String = client
        .query_one("SHOW server_version_num", &[])
        .expect("the PostgreSQL server version should be readable")
        .get(0);
    let version: i32 = version
        .parse()
        .expect("server_version_num should be an integer");

    assert!(
        (160_000..170_000).contains(&version),
        "expected PostgreSQL 16.x, got server_version_num={version}"
    );
}
