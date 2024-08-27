/*
 * Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License").
 * You may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

pub mod util;

use crate::util::assert_eq_cmd_json;
use assert_cmd::prelude::*;
use predicates::prelude::*;
use std::fs;
use std::path::PathBuf;
use tempfile::tempdir;
use tokio::io::AsyncWriteExt;

#[tokio::test]
async fn test_import_json() -> Result<(), Box<dyn std::error::Error>> {
    let mut tm = util::setup().await?;

    for format in ["json", "json-compact"] {
        let tbl = tm.create_temporary_table("pk", Some("sk,N")).await?;
        let base_dir = tempdir()?;
        let temp_path = base_dir.path().join(&tbl);

        // Write JSON to a file
        let contents = r#"[
        {"pk":"pk1","sk":1},
        {"pk":"pk1","sk":2,"a":1,"b":2},
        {"pk":"pk2","sk":3}
      ]"#;
        fs::write(&temp_path, contents)?;

        tm.command()?
            .args([
                "-r",
                "local",
                "import",
                "-t",
                &tbl,
                "-f",
                &format,
                "-i",
                &temp_path.to_str().unwrap(),
            ])
            .assert()
            .success()
            .stdout(predicate::str::contains("items processed"));

        // Test the imported data
        assert_eq_cmd_json(
            tm.command()?
                .args(["-r", "local", "get", "-t", &tbl, "pk1", "1"]),
            r#"{"pk":"pk1","sk":1}"#,
        );
        assert_eq_cmd_json(
            tm.command()?
                .args(["-r", "local", "get", "-t", &tbl, "pk1", "2"]),
            r#"{"pk":"pk1","sk":2,"a":1,"b":2}"#,
        );
        assert_eq_cmd_json(
            tm.command()?
                .args(["-r", "local", "get", "-t", &tbl, "pk2", "3"]),
            r#"{"pk":"pk2","sk":3}"#,
        );
    }

    Ok(())
}

#[tokio::test]
#[ignore]
async fn test_import_big_json() -> Result<(), Box<dyn std::error::Error>> {
    const INPUT_FILE_PATH: &'static str = "tests/resources/tmp/big_import_input.json";
    create_big_json_if_needed(INPUT_FILE_PATH).await?;

    let mut tm = util::setup().await?;
    let tbl = tm.create_temporary_table("pk,N", None).await?;
    tm.command()?
        .args([
            "-r",
            "local",
            "import",
            "-t",
            &tbl,
            "-f",
            "json",
            "-i",
            &INPUT_FILE_PATH,
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("items processed"));

    Ok(())
}

#[tokio::test]
async fn test_import_csv() -> Result<(), Box<dyn std::error::Error>> {
    let mut tm = util::setup().await?;
    let tbl = tm.create_temporary_table("pk", Some("sk,N")).await?;
    let base_dir = tempdir()?;
    let temp_path = base_dir.path().join(&tbl);

    // Write the CSV to a file
    let csv_contents = r#"pk,sk,a,b
"pk1",1,"a",true
"pk1",2,1,2
"pk2",3,null,null
"#;
    fs::write(&temp_path, csv_contents)?;

    // Import JSONL data
    tm.command()?
        .args([
            "-r",
            "local",
            "import",
            "-t",
            &tbl,
            "-f",
            "csv",
            "-i",
            &temp_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("items processed"));

    // Test the imported data
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk1", "1"]),
        r#"{"pk":"pk1","sk":1,"a":"a","b":true}"#,
    );
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk1", "2"]),
        r#"{"pk":"pk1","sk":2,"a":1,"b":2}"#,
    );
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk2", "3"]),
        r#"{"pk":"pk2","sk":3,"a":null,"b":null}"#,
    );

    Ok(())
}

#[tokio::test]
async fn test_import_jsonl() -> Result<(), Box<dyn std::error::Error>> {
    let mut tm = util::setup().await?;
    let tbl = tm.create_temporary_table("pk", Some("sk,N")).await?;
    let base_dir = tempdir()?;
    let temp_path = base_dir.path().join(&tbl);

    // Write the JSONL to a file
    let jsonl_contents = r#"{"pk":"pk1","sk":1}
{"pk":"pk1","sk":2,"a":1,"b":2}
{"pk":"pk2","sk":3}"#;
    println!("{}", jsonl_contents);
    fs::write(&temp_path, jsonl_contents)?;

    // Import JSONL data
    tm.command()?
        .args([
            "-r",
            "local",
            "import",
            "-t",
            &tbl,
            "-f",
            "jsonl",
            "-i",
            &temp_path.to_str().unwrap(),
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("items processed"));

    // Test the imported data
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk1", "1"]),
        r#"{"pk":"pk1","sk":1}"#,
    );
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk1", "2"]),
        r#"{"pk":"pk1","sk":2,"a":1,"b":2}"#,
    );
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk2", "3"]),
        r#"{"pk":"pk2","sk":3}"#,
    );

    Ok(())
}

#[tokio::test]
async fn test_import_jsonl_with_set_inference() -> Result<(), Box<dyn std::error::Error>> {
    let mut tm = util::setup().await?;
    let tbl = tm.create_temporary_table("pk", None).await?;
    let base_dir = tempdir()?;
    let temp_path = base_dir.path().join(&tbl);

    // Write the JSONL to a file
    let jsonl_contents = r#"{"pk":"pk1","nset":[1,2,3]}
{"pk":"pk2","sset":["1","2","3"]}
{"pk":"pk3","list":[1,"2",3]}"#;
    println!("{}", jsonl_contents);
    fs::write(&temp_path, jsonl_contents)?;

    // Import JSONL data
    tm.command()?
        .args([
            "-r",
            "local",
            "import",
            "-t",
            &tbl,
            "-f",
            "jsonl",
            "-i",
            &temp_path.to_str().unwrap(),
            "--enable-set-inference",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("items processed"));

    // Test the imported data
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk1", "-o", "raw"]),
        r#"{"pk":{"S":"pk1"},"nset":{"NS":["1","2","3"]}}"#,
    );
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk2", "-o", "raw"]),
        r#"{"pk":{"S":"pk2"},"sset":{"SS":["1","2","3"]}}"#,
    );
    assert_eq_cmd_json(
        tm.command()?
            .args(["-r", "local", "get", "-t", &tbl, "pk3", "-o", "raw"]),
        r#"{"pk":{"S":"pk3"},"list":{"L":[{"N":"1"},{"S":"2"},{"N":"3"}]}}"#,
    );

    Ok(())
}

async fn create_big_json_if_needed(json_path: &str) -> Result<(), tokio::io::Error> {
    const NUM_OF_ITEMS: i32 = 1_000_000;

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(json_path);

    if path.is_file() {
        return Ok(());
    }

    let file = tokio::fs::File::create(path)
        .await
        .expect(&format!("Failed to create a {}", json_path));
    let mut writer = tokio::io::BufWriter::new(file);
    writer.write(b"[").await?;
    for i in 0..NUM_OF_ITEMS {
        writer
            .write(format!("{{\"pk\":{},\"value\":\"value-{}\"}}", i, i).as_bytes())
            .await?;
        if i < NUM_OF_ITEMS - 1 {
            writer.write(b",\n").await?;
        }
    }
    writer.write(b"]\n").await?;
    writer.flush().await?;

    Ok(())
}
