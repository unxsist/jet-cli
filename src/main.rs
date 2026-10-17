mod api;
mod auth;
mod basket;
mod commands;
mod config;
mod discovery;
mod geo;
mod menu;
mod orders;
mod ui;

use clap::{Parser, Subcommand};

use discovery::ServiceType;

/// Order food from Just Eat Takeaway (thuisbezorgd.nl) from your terminal.
#[derive(Parser)]
#[command(name = "jet", version)]
struct Cli {
    /// Print machine-readable JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Log in with the email code flow.
    Login {
        /// Email address to log in with (defaults to the last one used).
        #[arg(long)]
        email: Option<String>,
        /// Log in with your normal browser and paste the redirect URL back.
        #[arg(long)]
        manual: bool,
        /// Ignore any remembered browser session and log in again.
        #[arg(long)]
        fresh: bool,
        /// Show the login window instead of running the browser in the background.
        #[arg(long)]
        show_browser: bool,
    },
    /// Forget the stored tokens.
    Logout {
        /// Also delete the login window's browser profile (cookies).
        #[arg(long)]
        forget_browser: bool,
    },
    /// Show the logged-in account.
    Whoami,
    /// Show or set the delivery address, e.g. `jet address "Damrak 1, Amsterdam"`.
    Address {
        /// Address or postcode + house number.
        query: Vec<String>,
    },
    /// List restaurants that deliver to your address.
    #[command(alias = "r")]
    Restaurants {
        /// Filter by name or cuisine.
        query: Vec<String>,
        /// Include restaurants that are closed right now.
        #[arg(long)]
        all: bool,
        /// Pickup instead of delivery.
        #[arg(long)]
        pickup: bool,
        #[arg(long, value_enum, default_value_t = Sort::Relevance)]
        sort: Sort,
        #[arg(short = 'n', long, default_value_t = 25)]
        limit: usize,
    },
    /// Show a restaurant's menu.
    #[command(alias = "m")]
    Menu {
        /// Restaurant slug or name (defaults to the basket's restaurant).
        restaurant: Option<String>,
        /// Only show items matching this text.
        #[arg(short, long)]
        filter: Option<String>,
        /// Pickup instead of delivery.
        #[arg(long)]
        pickup: bool,
    },
    /// Add an item to the basket. Prompts for anything not given on the command line.
    #[command(alias = "a")]
    Add {
        /// Item number from `jet menu`, or (part of) its name.
        item: Vec<String>,
        /// Restaurant slug or name (defaults to the basket's restaurant).
        #[arg(short, long)]
        restaurant: Option<String>,
        /// Size / variation, e.g. "large".
        #[arg(short, long)]
        size: Option<String>,
        /// Option to select (repeatable), e.g. -o rijst -o knoflooksaus.
        #[arg(short, long = "option")]
        options: Vec<String>,
        #[arg(short, long, default_value_t = 1)]
        quantity: u32,
        /// Note for the restaurant about this item.
        #[arg(long, default_value = "")]
        note: String,
        /// Pickup instead of delivery (only when starting a new basket).
        #[arg(long)]
        pickup: bool,
        /// Discard a basket at another restaurant without asking.
        #[arg(long)]
        new: bool,
    },
    /// Your recent orders.
    #[command(alias = "o")]
    Orders {
        #[arg(short = 'n', long, default_value_t = 10)]
        limit: usize,
    },
    /// Put the items of a previous order in the basket (1 = most recent).
    Reorder {
        #[arg(default_value_t = 1)]
        number: usize,
        /// Discard a basket at another restaurant without asking.
        #[arg(long)]
        new: bool,
    },
    /// Show or change the basket.
    #[command(alias = "b")]
    Basket {
        #[command(subcommand)]
        action: Option<BasketAction>,
    },
}

#[derive(Subcommand)]
pub enum BasketAction {
    /// Show the basket (default).
    Show,
    /// Remove a line, by its number in `jet basket`.
    Rm { line: usize },
    /// Change the quantity of a line.
    Qty { line: usize, quantity: u32 },
    /// Empty the basket.
    Clear,
}

#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Sort {
    Relevance,
    Rating,
    Eta,
    Fee,
    Distance,
}

pub fn service(pickup: bool) -> ServiceType {
    if pickup {
        ServiceType::Collection
    } else {
        ServiceType::Delivery
    }
}

#[tokio::main]
async fn main() {
    // Exit quietly when piped into `head` etc. instead of panicking on EPIPE.
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    let cli = Cli::parse();
    if let Err(e) = commands::run(cli.command, cli.json).await {
        eprintln!("{} {e:#}", console::style("error:").red().bold());
        std::process::exit(1);
    }
}
