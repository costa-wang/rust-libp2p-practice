use std::net::Ipv4Addr;
use parity_multiaddr::{Multiaddr, Protocol};

fn main(){
let address: Multiaddr = "/ip4/54.186.82.90/tcp/1347/p2p/12D3K1oWKNF7vNFEhnvB45E9mw2B5z6t419W3ziZPLdUDVnLLKGs".parse().unwrap();
// let components = address.iter().collect::<Vec<_>>();
// assert_eq!(components[0], Protocol::Ip4(Ipv4Addr::new(54, 186, 82, 90)));
// assert_eq!(components[1], Protocol::Tcp(1347));
// println!("{}",components[2]);
if let Some(Protocol::P2p(mh)) = address.pop(){
    // println!("{}",mh);
}
}