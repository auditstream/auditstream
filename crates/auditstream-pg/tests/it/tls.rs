use auditstream_testkit::TestDb;

fn with_sslmode_require(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&sslmode=require")
    } else {
        format!("{url}?sslmode=require")
    }
}

#[tokio::test]
async fn connect_negotiates_tls_instead_of_reporting_tls_disabled() {
    let db = TestDb::new().await.expect("create TestDb");
    let tls_url = with_sslmode_require(db.url());

    if let Err(err) = auditstream_pg::connect(&tls_url).await {
        let msg = err.to_string();
        assert!(
            !msg.contains("without TLS support enabled"),
            "sqlx must be compiled with rustls TLS enabled; got: {msg}"
        );
    }
}
