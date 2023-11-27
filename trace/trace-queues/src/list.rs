use anyhow::{anyhow, Result};
use cargo_files_core::get_target_files;
use cargo_metadata::MetadataCommand;
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;
use syn::visit::Visit;
use tracing::Level;

pub(crate) fn list_queues<P: Into<Option<PathBuf>>>(manifest_path: P) -> Result<()> {
    static QUEUE_MACRO_IDENTS: OnceLock<BTreeMap<&'static str, Level>> = OnceLock::new();
    static QUEUE_GROUP_MACRO_IDENTS: OnceLock<BTreeMap<&'static str, Level>> = OnceLock::new();

    struct QueueMacroVisitor {
        queues: BTreeMap<String, Level>,
        queue_groups: BTreeMap<String, Level>,
    }

    impl<'ast> Visit<'ast> for QueueMacroVisitor {
        fn visit_expr_macro(&mut self, i: &'ast syn::ExprMacro) {
            let queue_macro_idents = QUEUE_MACRO_IDENTS.get_or_init(|| {
                BTreeMap::from([
                    ("trace_queue", Level::TRACE),
                    ("debug_queue", Level::DEBUG),
                    ("info_queue", Level::INFO),
                    ("warn_queue", Level::WARN),
                    ("error_queue", Level::ERROR),
                ])
            });
            if queue_macro_idents
                .keys()
                .any(|ident| i.mac.path.is_ident(ident))
            {
                let name: String = i
                    .mac
                    .tokens
                    .clone()
                    .into_iter()
                    .nth(0)
                    .expect("Queue macro without name")
                    .to_string();
                let ident = i.mac.path.get_ident().unwrap();
                let level = queue_macro_idents.get(&*ident.to_string()).unwrap();
                if let Some(_) = self.queues.insert(name.clone(), *level) {
                    eprintln!("Queue with name '{}' already exists!", name);
                }
            }

            let queue_group_macro_idents = QUEUE_GROUP_MACRO_IDENTS.get_or_init(|| {
                BTreeMap::from([
                    ("trace_queue_group", Level::TRACE),
                    ("debug_queue_group", Level::DEBUG),
                    ("info_queue_group", Level::INFO),
                    ("warn_queue_group", Level::WARN),
                    ("error_queue_group", Level::ERROR),
                ])
            });
            if queue_group_macro_idents
                .keys()
                .any(|ident| i.mac.path.is_ident(ident))
            {
                let name = i
                    .mac
                    .tokens
                    .clone()
                    .into_iter()
                    .nth(0)
                    .expect("Queue macro without name")
                    .to_string();
                let ident = i.mac.path.get_ident().unwrap();
                let level = queue_group_macro_idents.get(&*ident.to_string()).unwrap();
                if let Some(_) = self.queue_groups.insert(name.clone(), *level) {
                    eprintln!("Queue Group with name '{}' already exists!", name);
                }
            }
        }
    }

    let mut visitor = QueueMacroVisitor {
        queues: BTreeMap::new(),
        queue_groups: BTreeMap::new(),
    };

    let mut cmd = MetadataCommand::new();
    if let Some(manifest_path) = manifest_path.into() {
        cmd.manifest_path(manifest_path);
    }
    let md = cmd.exec()?;
    let root_pkg = md.root_package().ok_or(anyhow!("No root package"))?;

    for target in &root_pkg.targets {
        let src_files = get_target_files(&cargo_files_core::Target::from_target(target))?;
        for src_file in src_files {
            let src_contents = fs::read_to_string(src_file)?;
            let parsed = syn::parse_file(&src_contents)?;
            visitor.visit_file(&parsed);
        }
    }

    println!(
        "{: <24}|{: <24}|{: <24}",
        "Queue Name", "Trace Level", "Is Group"
    );
    println!("{:=<24}|{:=<24}|{:=<24}", "", "", "");
    for (name, level) in visitor.queues {
        println!("{: <24}|{: <24}|{}", name, level, false);
    }
    for (name, level) in visitor.queue_groups {
        println!("{: <24}|{: <24}|{}", name, level, true);
    }

    Ok(())
}
