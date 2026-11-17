//! Tries the edge drop strip by hand:
//! `cargo run -p dnd --example edge_probe` – watches the edges that lead to
//!   another Deskflow screen (from %APPDATA%\Deskflow\Deskflow.conf) and prints
//!   file drags; after a drop it hands the cursor over through that edge.
//! `cargo run -p dnd --example edge_probe -- cross right` – only hands the
//!   cursor over (checks that Deskflow switches screens).
//! `cargo run -p dnd --example edge_probe -- watch right` – a given edge.

use std::time::Duration;

use dnd::{cross_edge, layout, Callbacks, Edge, EdgeDrop};

fn deskflow_conf() -> Option<String> {
    let base = std::env::var_os("APPDATA")?;
    std::fs::read_to_string(
        std::path::Path::new(&base)
            .join("Deskflow")
            .join("Deskflow.conf"),
    )
    .ok()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let edge_arg = args.get(1).and_then(|e| Edge::parse(e));
    if args.first().map(String::as_str) == Some("cross") {
        let edge = edge_arg.unwrap_or(Edge::Right);
        std::thread::sleep(Duration::from_millis(300));
        println!("cursor before: {:?}", dnd::cursor_position());
        match cross_edge(edge) {
            Ok(()) => println!("pushed the cursor out through the {edge:?} edge"),
            Err(e) => eprintln!("{e}"),
        }
        // Deskflow parks the server's hidden cursor in the middle after a switch
        for _ in 0..8 {
            std::thread::sleep(Duration::from_millis(60));
            println!("cursor: {:?}", dnd::cursor_position());
        }
        return;
    }

    let edges: Vec<Edge> = match edge_arg {
        Some(e) => vec![e],
        None => {
            let conf = deskflow_conf().unwrap_or_default();
            let me = layout::computer_name(&conf)
                .or_else(|| std::env::var("COMPUTERNAME").ok())
                .unwrap_or_default();
            let found = layout::from_grid(&conf, &me);
            for (edge, other) in &found {
                println!("{me}: {edge:?} edge -> {other}");
            }
            found.into_iter().map(|(e, _)| e).collect()
        }
    };
    if edges.is_empty() {
        eprintln!("no edge leads to another screen; pass one: watch right");
        std::process::exit(1);
    }
    let callbacks = Callbacks {
        on_hover: Box::new(|edge| match edge {
            Some(e) => println!("file drag at the {e:?} edge – release to send"),
            None => println!("file drag left the edge"),
        }),
        on_drop: Box::new(|d| {
            println!("dropped at {},{} on the {:?} edge:", d.x, d.y, d.edge);
            for f in &d.files {
                println!("  {}", f.display());
            }
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(120));
                if let Err(e) = cross_edge(d.edge) {
                    eprintln!("{e}");
                }
            });
        }),
    };
    let _watch = match EdgeDrop::start(edges.clone(), callbacks) {
        Ok(w) => w,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(1);
        }
    };
    println!("watching {edges:?}; drag a file from Explorer to the edge (Ctrl+C to quit)");
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
