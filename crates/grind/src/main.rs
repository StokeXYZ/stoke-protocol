//! Usage: meridian-grind <flags as decimal or 0x-hex> <out.json>
//! Finds an ed25519 keypair whose public key's last two bytes, masked to 14 bits, equal the flags.
//! Deploying a hook program at that address fixes its permissions forever, as in Uniswap v4.
use solana_keypair::Keypair;
use solana_signer::Signer;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("usage: meridian-grind <flags> <out.json>");
        std::process::exit(2);
    }
    let flags = if let Some(h) = args[1].strip_prefix("0x") {
        u16::from_str_radix(h, 16).expect("flags")
    } else {
        args[1].parse::<u16>().expect("flags")
    };
    assert!(flags < (1 << 14), "flags use 14 bits");
    let mut tries = 0u64;
    loop {
        tries += 1;
        let kp = Keypair::new();
        let b = kp.pubkey().to_bytes();
        if u16::from_be_bytes([b[30], b[31]]) & 0x3fff == flags {
            let json = format!("[{}]", kp.to_bytes().iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","));
            std::fs::write(&args[2], json).expect("write");
            println!("{} ({} tries)", kp.pubkey(), tries);
            return;
        }
    }
}
