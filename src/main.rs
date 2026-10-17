mod agent_login;
mod api;
mod auth;
mod basket;
mod checkout;
mod commands;
mod config;
mod discovery;
mod errors;
mod geo;
mod mcp;
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
        #[command(subcommand)]
        step: Option<LoginStep>,
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
    /// Check the setup: login, login window support, address, basket.
    Doctor,
    /// Move a login to another machine (e.g. an agent's sandbox).
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
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
    /// Run as an MCP server on stdio, for AI agents (e.g. `claude mcp add jet -- jet mcp`).
    Mcp,
    /// Get the basket ready to order: shows address, time, total and payment methods.
    Checkout {
        #[command(flatten)]
        details: OrderDetails,
    },
    /// Place the order; prints a payment link (and QR code) to finish paying.
    Order {
        #[command(subcommand)]
        action: Option<OrderAction>,
        /// Payment method: ideal, creditcard, paypal_rest, … (see `jet checkout`).
        #[arg(long)]
        pay: Option<String>,
        /// Order without asking, if the total is exactly this amount (for scripts/agents).
        #[arg(long)]
        confirm_total: Option<f64>,
        /// Open the payment page in your browser.
        #[arg(long)]
        open: bool,
        #[command(flatten)]
        details: OrderDetails,
    },
    /// Show or change the basket.
    #[command(alias = "b")]
    Basket {
        #[command(subcommand)]
        action: Option<BasketAction>,
    },
}

/// Two-step login for agents and scripts.
#[derive(Subcommand)]
pub enum LoginStep {
    /// Send a login code and wait in the background for `jet login code`.
    Start {
        #[arg(long)]
        email: Option<String>,
        /// Ignore any remembered session and log in again.
        #[arg(long)]
        fresh: bool,
    },
    /// Finish a login: the emailed code (after `start`), or the address of the page you ended on
    /// (after `link`).
    Code { code: String },
    /// Log in on another device (phone, laptop): prints a link; afterwards pass the address of the
    /// page you end on to `jet login code`. For servers and sandboxes without a screen.
    Link {
        /// Ignore any remembered session and log in again.
        #[arg(long)]
        fresh: bool,
    },
    /// Internal: the background half of `jet login start`.
    #[command(hide = true)]
    Daemon {
        #[arg(long)]
        email: String,
        #[arg(long)]
        fresh: bool,
    },
}

#[derive(clap::Args)]
pub struct OrderDetails {
    /// Delivery time: "asap" or e.g. "19:30" (first slot from then).
    #[arg(long)]
    pub time: Option<String>,
    /// Note for the courier (delivery) or restaurant (pickup).
    #[arg(long)]
    pub note: Option<String>,
    /// Your name for the order (remembered; default: from your last order).
    #[arg(long)]
    pub name: Option<String>,
    /// Your phone number for the order (remembered; default: from your last order).
    #[arg(long)]
    pub phone: Option<String>,
}

#[derive(Subcommand)]
pub enum OrderAction {
    /// Has the last order been paid and placed?
    Status,
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Print this login as a string for `jet auth import` or the JET_SESSION environment variable.
    /// Treat it like a password.
    Export,
    /// Log in with a string from `jet auth export`.
    Import { session: String },
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
    let json = cli.json;
    let result = match cli.command {
        Command::Mcp => mcp::serve().await,
        command => commands::run(command, json).await,
    };
    if let Err(e) = result {
        let kind = e.downcast_ref::<errors::JetError>().map(|j| j.kind);
        if json {
            let error = serde_json::json!({
                "error": { "kind": kind.map_or("error", |k| k.name()), "message": format!("{e:#}") }
            });
            println!("{error}");
        } else {
            eprintln!("{} {e:#}", console::style("error:").red().bold());
        }
        std::process::exit(kind.map_or(1, |k| k.exit_code()));
    }
}
