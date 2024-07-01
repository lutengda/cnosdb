use core::panic;
use std::{env, fs, io::Write, path::PathBuf, process::Command};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let project_root_dir = env::current_dir()?;
    let proto_files_dir = project_root_dir.join("proto");
    let prompb_proto_files_dir = project_root_dir.join("prompb");

    // src/generated/mod.rs
    let generated_mod_rs_path = project_root_dir
        .join("src")
        .join("generated")
        .join("mod.rs");
    let mut generated_mod_rs_file = fs::File::create(generated_mod_rs_path)?;
    generated_mod_rs_file.write_all(
        b"#![allow(unused_imports)]
#![allow(clippy::all)]
mod protobuf_generated;
pub use protobuf_generated::*;

mod flatbuffers_generated;
",
    )?;

    // build .proto files
    {
        let proto_file_paths = &[
            proto_files_dir.join("kv_service.proto"),
            proto_files_dir.join("vector_event.proto"),
            proto_files_dir.join("raft_service.proto"),
            proto_files_dir.join("jaeger_api_v2.proto"),
            proto_files_dir.join("jaeger_storage_v1.proto"),
        ];
        let rust_mod_names = &[
            "kv_service".to_string(),
            "vector".to_string(),
            "raft_service".to_string(),
            "jaeger_api_v2".to_string(),
            "jaeger_storage_v1".to_string(),
        ];

        // src/generated/protobuf_generated/
        let output_dir_final = env::current_dir()
            .unwrap()
            .join("src")
            .join("generated")
            .join("protobuf_generated");
        fs::create_dir_all(&output_dir_final)?;
        let descriptor_set_path =
            PathBuf::from(env::var("OUT_DIR").unwrap()).join("proto-descriptor.bin");

        tonic_build::configure()
            .out_dir(&output_dir_final)
            .file_descriptor_set_path(descriptor_set_path)
            .protoc_arg("--experimental_allow_proto3_optional")
            .compile_well_known_types(true)
            .compile(proto_file_paths, &[proto_files_dir.as_path()])
            .expect("Failed to generate protobuf file {}.");
        eprintln!("Generated protobuf files in {:?}", output_dir_final);

        // src/generated/protobuf_generated/mod.rs
        let mut protobuf_generated_mod_rs_file = fs::File::create(output_dir_final.join("mod.rs"))?;
        for mod_name in rust_mod_names.iter() {
            if mod_name == "jaeger_storage_v1" {
                protobuf_generated_mod_rs_file
                    .write_all("#[path = \"jaeger.storage.v1.rs\"]".as_bytes())?;
                protobuf_generated_mod_rs_file.write_all(b"\n")?;
            }
            protobuf_generated_mod_rs_file.write_all(b"pub mod ")?;
            protobuf_generated_mod_rs_file.write_all(mod_name.as_bytes())?;
            protobuf_generated_mod_rs_file.write_all(b";\n")?;
            protobuf_generated_mod_rs_file.flush()?;
        }
    }

    // build prompb/**.proto files
    {
        let proto_file_paths = &[
            prompb_proto_files_dir.join("types.proto"),
            prompb_proto_files_dir.join("remote.proto"),
        ];
        let rust_mod_names = &["prometheus".to_string()];

        // src/generated/protobuf_generated/
        let output_dir_final = env::current_dir().unwrap().join("src").join("prompb");
        fs::create_dir_all(&output_dir_final)?;
        let descriptor_set_path =
            PathBuf::from(env::var("OUT_DIR").unwrap()).join("proto-descriptor.bin");

        tonic_build::configure()
            .out_dir(&output_dir_final)
            .file_descriptor_set_path(descriptor_set_path)
            .compile_well_known_types(true)
            .compile(proto_file_paths, &[prompb_proto_files_dir.as_path()])
            .expect("Failed to generate protobuf file {}.");
        eprintln!("Generated protobuf files in {:?}", output_dir_final);

        // let output_file_path = output_dir_final.join("prometheus.rs");
        // format_file(&output_file_path);

        // src/prompb/mod.rs
        let mut protobuf_generated_mod_rs_file = fs::File::create(output_dir_final.join("mod.rs"))?;
        for mod_name in rust_mod_names.iter() {
            protobuf_generated_mod_rs_file.write_all(b"pub mod ")?;
            protobuf_generated_mod_rs_file.write_all(mod_name.as_bytes())?;
            protobuf_generated_mod_rs_file.write_all(b";\n")?;
            protobuf_generated_mod_rs_file.flush()?;
        }
    }

    // build .fbs files
    {
        let fbs_file_paths = &[proto_files_dir.join("models.fbs")];

        // src/generated/flatbuffers_generated/
        let output_dir_final = env::current_dir()
            .unwrap()
            .join("src")
            .join("generated")
            .join("flatbuffers_generated");
        fs::create_dir_all(&output_dir_final)?;

        // src/generated/flatbuffers_generated/mod.rs
        let mut flatbuffers_generated_mod_rs_file =
            fs::File::create(output_dir_final.join("mod.rs"))?;

        // <flatbuffers_file_name>.fbs -> <flatbuffers_file_name>
        for p in fbs_file_paths.iter() {
            let output_rust_mod_name = p
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .split('.')
                .collect::<Vec<&str>>()
                .first()
                .unwrap()
                .to_string();

            // <flatbuffers_file_name> -> <flatbuffers_file_name>.rs
            let output_file_name = output_rust_mod_name.clone() + ".rs";

            generated_mod_rs_file.write_all(b"pub use flatbuffers_generated::")?;
            generated_mod_rs_file.write_all(output_rust_mod_name.as_bytes())?;
            generated_mod_rs_file.write_all(b"::*;\n")?;
            generated_mod_rs_file.flush()?;

            flatbuffers_generated_mod_rs_file.write_all(b"pub mod ")?;
            flatbuffers_generated_mod_rs_file.write_all(output_rust_mod_name.as_bytes())?;
            flatbuffers_generated_mod_rs_file.write_all(b";\n")?;
            flatbuffers_generated_mod_rs_file.flush()?;

            let flatc_path = match env::var("FLATC_PATH") {
                Ok(p) => {
                    eprintln!(
                        "Found specified flatc path in environment FLATC_PATH( {} )",
                        &p
                    );
                    p
                }
                Err(_) => "flatc".to_string(),
            };
            let output = Command::new(&flatc_path)
                .arg("-o")
                .arg(&output_dir_final)
                .arg("--rust")
                .arg("--gen-mutable")
                .arg("--gen-onefile")
                .arg("--gen-name-strings")
                .arg("--filename-suffix")
                .arg("")
                .arg(p)
                .output()
                .unwrap_or_else(|e| {
                    panic!(
                        "Failed to generate file '{}' by flatc(path: '{}'): {:?}.",
                        output_file_name, flatc_path, e
                    )
                });

            if !output.status.success() {
                panic!("{}", String::from_utf8(output.stderr).unwrap());
            }
            eprintln!("Generated flatbuffers files in {:?}", output_dir_final);
        }
    }

    Ok(())
}
