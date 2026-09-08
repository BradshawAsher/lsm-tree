use std::env;
use std::process::exit;

use lsm_tree::LsmEngine;

fn print_usage() {
    println!("LSM-Tree Interactive CLI Tool");
    println!("Usage:");
    println!("  cargo run --bin cli -- put <key> <value>   Insert or update a key");
    println!("  cargo run --bin cli -- get <key>           Query value for key");
    println!("  cargo run --bin cli -- delete <key>        Delete a key (write tombstone)");
    println!("  cargo run --bin cli -- compact             Trigger background compaction");
    println!("  cargo run --bin cli -- status              Print database storage statistics");
    println!();
    println!("Data directory: ./lsm_data");
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        print_usage();
        exit(1);
    }

    let db_path = "./lsm_data";
    let db = LsmEngine::open(db_path)?;

    match args[1].as_str() {
        "put" => {
            if args.len() < 4 {
                eprintln!("Error: 'put' requires <key> and <value>");
                exit(1);
            }
            let key = args[2].as_bytes();
            let val = args[3].as_bytes();
            db.put(key, val)?;
            println!("✅ OK (Stored key: '{}')", args[2]);
        }
        "get" => {
            if args.len() < 3 {
                eprintln!("Error: 'get' requires <key>");
                exit(1);
            }
            let key = args[2].as_bytes();
            match db.get(key)? {
                Some(val) => println!("{}", String::from_utf8_lossy(&val)),
                None => {
                    println!("(nil) Key not found");
                    exit(1);
                }
            }
        }
        "delete" => {
            if args.len() < 3 {
                eprintln!("Error: 'delete' requires <key>");
                exit(1);
            }
            let key = args[2].as_bytes();
            db.delete(key)?;
            println!("✅ OK (Deleted key: '{}')", args[2]);
        }
        "compact" => {
            let before = db.sstable_count();
            println!("Running multi-way merge compaction across {} SSTables...", before);
            db.compact()?;
            let after = db.sstable_count();
            println!("✅ Compaction complete: {} -> {} SSTable(s)", before, after);
        }
        "status" => {
            println!("=== Database Status ===");
            println!("Path: {}", db.dir().display());
            println!("Active MemTable size: {} bytes", db.memtable_size());
            println!("SSTable count on disk: {}", db.sstable_count());
        }
        _ => {
            print_usage();
            exit(1);
        }
    }

    Ok(())
}
