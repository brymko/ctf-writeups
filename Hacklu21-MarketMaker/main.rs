#![allow(unused)]
use std::collections::BTreeMap;
use std::net::{SocketAddr, UdpSocket};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Debug, Default)]
pub(crate) struct MThread<T> {
    ptr: Arc<Mutex<T>>,
}

impl<T> Clone for MThread<T> {
    fn clone(&self) -> Self {
        Self {
            ptr: self.ptr.clone(),
        }
    }
}

impl<T> MThread<T>
where
    T: Sized,
{
    pub(crate) fn new(t: T) -> Self {
        Self {
            ptr: Arc::new(Mutex::new(t)),
        }
    }

    pub(crate) fn get(&self) -> MutexGuard<'_, T> {
        // UNWRAP: mtx poison == unrecoverable error
        self.ptr.lock().unwrap()
    }
}

#[derive(Debug)]
struct Client {
    pub addr: SocketAddr,
    pub money: f64,
    pub is_market_maker: bool,
    pub net_liquidity_contribution: isize,
    pub position: isize,
    pub cycles_present: isize,
}

impl Client {
    pub fn new(addr: SocketAddr) -> Self {
        let money = if addr.ip().is_loopback() {
            1e9
        } else {
            10000.0
        };
        Self {
            cycles_present: 0,
            addr,
            money,
            is_market_maker: false,
            net_liquidity_contribution: 0,
            position: 0,
        }
    }

    pub fn to_bytes(&self) -> [u8; 25] {
        let mut ret = [0; 25];

        ret[0] = 0x21;

        ret[1..9].copy_from_slice(&self.money.to_le_bytes()[..]);
        ret[9..17].copy_from_slice(&self.net_liquidity_contribution.to_le_bytes()[..]);
        ret[17..25].copy_from_slice(&self.position.to_le_bytes()[..]);

        ret
    }
}

#[derive(Debug)]
struct LimitOrder {
    lmt: f64,
    amount: isize,
}

impl LimitOrder {
    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() <= 16 {
            return None;
        }

        let lmt = f64::from_le_bytes(buf[0..8].try_into().unwrap());
        let amount = isize::from_le_bytes(buf[8..16].try_into().unwrap());

        if lmt.is_nan() || lmt.is_sign_negative() {
            return None;
        }

        if amount.abs() > 10000 {
            return None;
        }

        Some(Self { lmt, amount })
    }
}

#[derive(Debug)]
struct MarketOrder {
    amount: isize,
}

impl MarketOrder {
    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() <= 8 {
            return None;
        }

        let amount = isize::from_le_bytes(buf[0..8].try_into().unwrap());

        if amount.abs() > 10000 {
            return None;
        }

        Some(Self { amount })
    }
}

#[derive(Debug)]
struct CancleOrder {
    order_id: isize,
}

impl CancleOrder {
    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() <= 8 {
            return None;
        }

        let order_id = isize::from_le_bytes(buf[0..8].try_into().unwrap());

        Some(Self { order_id })
    }
}

#[derive(Debug)]
struct HiddenOrder {
    lmt: f64,
    amount: isize,
}

impl HiddenOrder {
    fn from_bytes(buf: &[u8]) -> Option<Self> {
        if buf.len() <= 16 {
            return None;
        }

        let amount = isize::from_le_bytes(buf[0..8].try_into().unwrap());
        let lmt = f64::from_le_bytes(buf[8..16].try_into().unwrap());

        Some(Self { amount, lmt })
    }
}

#[derive(Debug)]
struct BookEntry {
    client: SocketAddr,
    amount: isize,
    id: isize,
    cycles_present: isize,
}

#[derive(Debug)]
struct OrderBook {
    bids: BTreeMap<isize, Vec<BookEntry>>,
    asks: BTreeMap<isize, Vec<BookEntry>>,
    inc_id: isize,
}

const FLOATING_TO_FIXED_OFF: f64 = 1000.0;

impl OrderBook {
    fn new() -> Self {
        Self {
            bids: BTreeMap::new(),
            asks: BTreeMap::new(),
            inc_id: 0,
        }
    }

    fn do_hidden(
        &mut self,
        clients: Clients,
        socket: &mut UdpSocket,
        ordering_client: SocketAddr,
        mut order: HiddenOrder,
    ) -> Result<(), ()> {
        let mut lock = clients.get();

        if order.amount.is_negative() {
            let nbbo = self.bids.iter().next();
            match nbbo {
                None => return Err(()),
                Some((nbbo, _)) => {
                    let as_f64 = (*nbbo as f64 / FLOATING_TO_FIXED_OFF);
                    if as_f64 * order.amount as f64 > lock.get(&ordering_client).unwrap().money {
                        return Err(());
                    }
                }
            }

            order.amount = order.amount.abs();
            for (bid, entries) in self.bids.iter_mut().rev() {
                let bid_as_f64 = (*bid as f64 / FLOATING_TO_FIXED_OFF);
                for entry in entries {
                    if entry.amount == 0 {
                        continue;
                    }
                    let sell_amt = if entry.amount >= order.amount {
                        order.amount
                    } else {
                        entry.amount
                    };
                    let price = bid_as_f64;

                    entry.amount -= sell_amt;
                    order.amount -= sell_amt;

                    let buyer = lock.get_mut(&entry.client).unwrap();
                    buyer.money -= sell_amt as f64 * price;
                    buyer.position += sell_amt;
                    buyer.net_liquidity_contribution += 1;
                    buyer.is_market_maker = buyer.net_liquidity_contribution >= 100;

                    let lmtexec = LmtExecution {
                        order_id: entry.id,
                        amount: sell_amt,
                        price,
                    };
                    socket.send_to(&lmtexec.to_bytes(), buyer.addr);

                    let oc = lock.get_mut(&ordering_client).unwrap();
                    oc.money -= sell_amt as f64 * bid_as_f64;
                    oc.position += sell_amt;
                    oc.is_market_maker = oc.net_liquidity_contribution >= 100;
                    let er = OrderResponse::Market(MarketResponse {
                        amount: sell_amt,
                        price,
                    });
                    socket.send_to(&er.to_bytes(), ordering_client);

                    if order.amount <= 0 {
                        return Ok(());
                    }
                }
            }
        } else {
            if lock.get(&ordering_client).unwrap().position < order.amount.abs() {
                return Err(());
            }

            for (ask, entries) in self.asks.iter_mut() {
                let ask_as_f64 = (*ask as f64 / FLOATING_TO_FIXED_OFF);
                for entry in entries {
                    if entry.amount == 0 {
                        continue;
                    }
                    let buy_amt = if entry.amount <= order.amount {
                        entry.amount
                    } else {
                        order.amount
                    };

                    let price = ask_as_f64;

                    entry.amount -= buy_amt;
                    order.amount -= buy_amt;

                    if let Some(seller) = lock.get_mut(&entry.client) {
                        seller.money += buy_amt as f64 * price;
                        seller.position -= buy_amt;
                        seller.net_liquidity_contribution += 1;
                        seller.is_market_maker = seller.net_liquidity_contribution >= 100;

                        let lmtexec = LmtExecution {
                            order_id: entry.id,
                            amount: buy_amt,
                            price,
                        };
                        socket.send_to(&lmtexec.to_bytes(), seller.addr);
                    }

                    if let Some(oc) = lock.get_mut(&ordering_client) {
                        oc.money += buy_amt as f64 * price;
                        oc.position -= buy_amt;
                        oc.is_market_maker = oc.net_liquidity_contribution >= 100;
                        let er = OrderResponse::Market(MarketResponse {
                            amount: buy_amt,
                            price,
                        });
                        socket.send_to(&er.to_bytes(), ordering_client);
                    }

                    if order.amount <= 0 {
                        return Ok(());
                    }
                }
            }
        }

        if order.amount != 0 {
            Err(())
        } else {
            Ok(())
        }
    }

    fn do_lmt(
        &mut self,
        clients: Clients,
        socket: &mut UdpSocket,
        ordering_client: SocketAddr,
        order: LimitOrder,
    ) -> Result<(), ()> {
        let lock = clients.get();
        let c = lock.get(&ordering_client).unwrap();
        let price = (order.lmt * FLOATING_TO_FIXED_OFF) as isize;
        let id = (price << 24) + self.inc_id;
        self.inc_id += 1;
        let be = BookEntry {
            client: ordering_client,
            amount: order.amount.abs(),
            id,
            cycles_present: 0,
        };
        if order.amount.is_negative() {
            if order.amount.abs() > c.position {
                return Err(());
            }
            self.asks.entry(price).or_insert_with(Vec::new).push(be);
        } else {
            if order.lmt * order.amount as f64 > c.money {
                return Err(());
            }
            self.bids.entry(price).or_insert_with(Vec::new).push(be);
        }

        let res = OrderResponse::Lmt(LmtResponse { order_id: id });
        socket.send_to(&res.to_bytes(), c.addr);

        Ok(())
    }

    fn do_mkt(
        &mut self,
        clients: Clients,
        socket: &mut UdpSocket,
        ordering_client: SocketAddr,
        mut order: MarketOrder,
    ) -> Result<(), ()> {
        let mut lock = clients.get();

        if order.amount.is_negative() {
            if lock.get(&ordering_client).unwrap().position < order.amount.abs() {
                return Err(());
            }
            order.amount = order.amount.abs();
            for (bid, entries) in self.bids.iter_mut().rev() {
                let bid_as_f64 = (*bid as f64 / FLOATING_TO_FIXED_OFF);
                for entry in entries {
                    if entry.amount == 0 {
                        continue;
                    }
                    let sell_amt = if entry.amount >= order.amount {
                        order.amount
                    } else {
                        entry.amount
                    };
                    let price = bid_as_f64;

                    entry.amount -= sell_amt;
                    order.amount -= sell_amt;

                    if let Some(buyer) = lock.get_mut(&entry.client) {
                        buyer.money -= sell_amt as f64 * price;
                        buyer.position += sell_amt;
                        buyer.net_liquidity_contribution += 1;
                        buyer.is_market_maker = buyer.net_liquidity_contribution >= 100;

                        let lmtexec = LmtExecution {
                            order_id: entry.id,
                            amount: sell_amt,
                            price,
                        };
                        socket.send_to(&lmtexec.to_bytes(), buyer.addr);
                    }

                    if let Some(oc) = lock.get_mut(&ordering_client) {
                        oc.money += sell_amt as f64 * bid_as_f64;
                        oc.position -= sell_amt;
                        oc.net_liquidity_contribution -= 1;
                        oc.is_market_maker = oc.net_liquidity_contribution >= 100;
                        let er = OrderResponse::Market(MarketResponse {
                            amount: sell_amt,
                            price,
                        });
                        socket.send_to(&er.to_bytes(), ordering_client);
                    }

                    if order.amount <= 0 {
                        return Ok(());
                    }
                }
            }
        } else {
            let nbbo = self.asks.iter().next();
            if let Some((nbbo, _)) = nbbo {
                let as_f64 = (*nbbo as f64 / FLOATING_TO_FIXED_OFF);
                if as_f64 * order.amount as f64 > lock.get(&ordering_client).unwrap().money {
                    return Err(());
                }
            }

            for (ask, entries) in self.asks.iter_mut() {
                let ask_as_f64 = (*ask as f64 / FLOATING_TO_FIXED_OFF);
                for entry in entries {
                    if entry.amount == 0 {
                        continue;
                    }
                    let buy_amt = if entry.amount <= order.amount {
                        entry.amount
                    } else {
                        order.amount
                    };

                    let price = ask_as_f64;

                    entry.amount -= buy_amt;
                    order.amount -= buy_amt;

                    if let Some(seller) = lock.get_mut(&entry.client) {
                        seller.money += buy_amt as f64 * price;
                        seller.position -= buy_amt;
                        seller.net_liquidity_contribution += 1;
                        seller.is_market_maker = seller.net_liquidity_contribution >= 100;

                        let lmtexec = LmtExecution {
                            order_id: entry.id,
                            amount: buy_amt,
                            price,
                        };
                        socket.send_to(&lmtexec.to_bytes(), seller.addr);
                    }

                    if let Some(oc) = lock.get_mut(&ordering_client) { 
                        oc.money -= buy_amt as f64 * price;
                        oc.position += buy_amt;
                        oc.net_liquidity_contribution -= 1;
                        oc.is_market_maker = oc.net_liquidity_contribution >= 100;
                        let er = OrderResponse::Market(MarketResponse {
                            amount: buy_amt,
                            price,
                        });
                        socket.send_to(&er.to_bytes(), ordering_client);
                    }

                    if order.amount <= 0 {
                        return Ok(());
                    }
                }
            }

            if order.amount != 0 {
                if let Some(oc) = lock.get_mut(&ordering_client) {
                    oc.money -= order.amount as f64 * 1.0;
                    oc.position += order.amount;
                    order.amount = 0;
                    let er = OrderResponse::Market(MarketResponse {
                        amount: order.amount,
                        price: 1.0,
                    });
                    socket.send_to(&er.to_bytes(), ordering_client);
                }
            }
        }

        if order.amount != 0 {
            Err(())
        } else {
            Ok(())
        }
    }

    fn do_cncl(
        &mut self,
        socket: &mut UdpSocket,
        ordering_client: SocketAddr,
        cncl: CancleOrder,
    ) -> Result<(), ()> {
        let price = cncl.order_id >> 24;
        // let price = (order.lmt * FLOATING_TO_FIXED_OFF) as isize;

        if let Some(entries) = self.bids.get_mut(&price) {
            if let Some(idx) = entries.iter().enumerate().find_map(|(i, bid)| {
                if bid.id == cncl.order_id && ordering_client == bid.client {
                    Some(i)
                } else {
                    None
                }
            }) {
                entries.remove(idx);
                socket.send_to(&[0xe0], ordering_client);
                return Ok(());
            }
        }

        if let Some(entries) = self.asks.get_mut(&price) {
            if let Some(idx) = entries.iter().enumerate().find_map(|(i, ask)| {
                if ask.id == cncl.order_id && ordering_client == ask.client {
                    Some(i)
                } else {
                    None
                }
            }) {
                entries.remove(idx);
                socket.send_to(&[0xe0], ordering_client);
                return Ok(());
            }
        }

        Err(())
    }
}

#[derive(Debug)]
struct LmtResponse {
    order_id: isize,
}
#[derive(Debug)]
struct MarketResponse {
    amount: isize,
    price: f64,
}
#[derive(Debug)]
struct CnclResponse {
    cancled: bool,
    order_id: isize,
}
#[derive(Debug)]
struct HiddenResponse {
    order_id: isize,
}

#[derive(Debug)]
enum OrderResponse {
    Lmt(LmtResponse),
    Market(MarketResponse),
    Cncl(CnclResponse),
    Hidden(HiddenResponse),
}

impl OrderResponse {
    fn to_bytes(&self) -> [u8; 18] {
        let mut res = [0; 18];
        res[0] = 1;
        match self {
            OrderResponse::Lmt(r) => {
                res[1] = 0;
                res[2..10].copy_from_slice(&r.order_id.to_le_bytes()[..]);
            }
            OrderResponse::Cncl(r) => {
                res[1] = 1;
                res[2] = if r.cancled { 1 } else { 0 };
                res[3..11].copy_from_slice(&r.order_id.to_le_bytes()[..]);
            }
            OrderResponse::Market(r) => {
                res[1] = 2;
                res[2..10].copy_from_slice(&r.amount.to_le_bytes()[..]);
                res[10..18].copy_from_slice(&r.price.to_le_bytes()[..]);
            }
            OrderResponse::Hidden(r) => {
                res[1] = 3;
                res[2..10].copy_from_slice(&r.order_id.to_le_bytes()[..]);
            }
        }
        res
    }
}

#[derive(Debug)]
enum Order {
    Lmt(LimitOrder),
    Market(MarketOrder),
    Cncl(CancleOrder),
    Hidden(HiddenOrder),
}

struct LmtExecution {
    order_id: isize,
    amount: isize,
    price: f64,
}

impl LmtExecution {
    fn to_bytes(&self) -> [u8; 25] {
        let mut res = [0; 25];

        res[0] = 0x20;
        res[1..9].copy_from_slice(&self.order_id.to_le_bytes()[..]);
        res[9..17].copy_from_slice(&self.amount.to_le_bytes()[..]);
        res[17..25].copy_from_slice(&self.price.to_le_bytes()[..]);

        res
    }
}

type Clients = MThread<BTreeMap<SocketAddr, Client>>;

fn client_rx(socket: UdpSocket, clients: Clients, order_sender: Sender<(SocketAddr, Order)>) {
    const BUFFER_LEN: usize = 2048;
    let mut buffer = [0u8; BUFFER_LEN];
    loop {
        buffer.iter_mut().for_each(|b| *b = 0);
        if let Ok((bytes, addr)) = socket.recv_from(&mut buffer) {
            if bytes >= BUFFER_LEN {
                continue;
            }

            if buffer[0] == 0x69 {
                clients.get().remove(&addr);
                continue;
            }

            let is_mm = clients
                .get()
                .entry(addr)
                .or_insert_with(|| Client::new(addr))
                .is_market_maker;

            let order = move || -> Option<Order> {
                Some(match buffer[0] {
                    0 => Order::Lmt(LimitOrder::from_bytes(&buffer[1..])?),
                    1 => Order::Market(MarketOrder::from_bytes(&buffer[1..])?),
                    2 => Order::Cncl(CancleOrder::from_bytes(&buffer[1..])?),
                    3 if is_mm => Order::Hidden(HiddenOrder::from_bytes(&buffer[1..])?),
                    _ => None?,
                })
            }();

            if let Some(order) = order {
                order_sender.send((addr, order));
            }
        }
    }
}

fn main() {
    let flag = std::fs::read_to_string("flag").unwrap();
    let (order_sender, orders) = channel();
    let mut socket = UdpSocket::bind("0.0.0.0:14550").unwrap();
    let tsocket = socket.try_clone().unwrap();
    let clients = Clients::new(BTreeMap::new());
    let tclients = clients.clone();
    let mut order_book = OrderBook::new();

    std::thread::spawn(move || client_rx(tsocket, tclients, order_sender));

    let order_waiter = std::time::Duration::from_millis(10);
    loop {
        let now = std::time::Instant::now();

        while now.elapsed().subsec_millis() < 500 {
            if let Ok((caddr, order)) = orders.recv_timeout(order_waiter) {
                match order {
                    Order::Lmt(lmt) => {
                        if order_book
                            .do_lmt(clients.clone(), &mut socket, caddr, lmt)
                            .is_err()
                        {
                            socket.send_to(&[0xff], caddr);
                        }
                    }
                    Order::Market(mkt) => {
                        if order_book
                            .do_mkt(clients.clone(), &mut socket, caddr, mkt)
                            .is_err()
                        {
                            socket.send_to(&[0xfe], caddr);
                        }
                    }
                    Order::Cncl(cncl) => {
                        if order_book.do_cncl(&mut socket, caddr, cncl).is_err() {
                            socket.send_to(&[0xfd], caddr);
                        }
                    }
                    Order::Hidden(hid) => {
                        if order_book
                            .do_hidden(clients.clone(), &mut socket, caddr, hid)
                            .is_err()
                        {
                            socket.send_to(&[0xfc], caddr);
                        }
                    }
                }
            }
        }

        clients.get().retain(|addr, client| {
            if client.money >= 10000000.0
                && client.is_market_maker
                && !client.addr.ip().is_loopback()
            {
                socket.send_to(flag.as_bytes(), addr);
                false
            } else if client.money <= 10.0 || client.cycles_present > 2 * 30 * 60 {
                socket.send_to(&[0x69], addr);
                false
            } else {
                true
            }
        });

        {
            let mut lock = clients.get();
            'outer: for (strike, bidbook) in order_book.bids.iter_mut() {
                let strike_asf64 = (*strike as f64 / FLOATING_TO_FIXED_OFF);
                if let Some(askbook) = order_book.asks.get_mut(strike) {
                    let mut asksentryiter = askbook.iter_mut();
                    let mut cur_ask_entry: Option<&mut BookEntry> = asksentryiter.next();
                    for (booki, bid_entry) in bidbook.iter_mut().enumerate() {
                        while bid_entry.amount != 0 {
                            if cur_ask_entry.is_none()
                                || cur_ask_entry.as_ref().unwrap().amount == 0
                            {
                                cur_ask_entry = asksentryiter.next();
                            }

                            if cur_ask_entry.is_none() {
                                continue 'outer;
                            }
                            let ask_entry = cur_ask_entry.as_mut().unwrap();

                            let trade_amt = if bid_entry.amount >= ask_entry.amount {
                                ask_entry.amount
                            } else {
                                bid_entry.amount
                            };
                            let price = strike_asf64;

                            bid_entry.amount -= trade_amt;
                            ask_entry.amount -= trade_amt;

                                let lmtexec = LmtExecution {
                                    order_id: bid_entry.id,
                                    amount: trade_amt,
                                    price,
                                };

                            if let Some(buyer) = lock.get_mut(&bid_entry.client) { 
                                buyer.money -= trade_amt as f64 * price;
                                buyer.position += trade_amt;
                                buyer.net_liquidity_contribution += 1;
                                buyer.is_market_maker = buyer.net_liquidity_contribution >= 100;

                                socket.send_to(&lmtexec.to_bytes(), buyer.addr);
                            }

                            if let Some(seller) = lock.get_mut(&ask_entry.client) {
                                seller.money += trade_amt as f64 * price;
                                seller.position -= trade_amt;
                                seller.net_liquidity_contribution += 1;
                                seller.is_market_maker = seller.net_liquidity_contribution >= 100;

                                socket.send_to(&lmtexec.to_bytes(), seller.addr);
                            }
                        }
                    }
                }
            }

            order_book.bids.iter_mut().for_each(|(_, lvl2)| {
                lvl2.retain(|entry| entry.amount != 0 && lock.get(&entry.client).is_some());
            });

            order_book.asks.iter_mut().for_each(|(_, lvl2)| {
                lvl2.retain(|entry| entry.amount != 0 && lock.get(&entry.client).is_some());
            });

            let mut buffer = [0u8; 0x1000];
            let mut idx = 1;
            buffer[0] = 0xc1;
            for (strike, lvl2) in order_book.bids.iter_mut().rev() {
                let volume: isize = lvl2
                    .iter_mut()
                    .map(|entry| {
                        entry.cycles_present += 1;
                        entry.amount
                    })
                    .sum();
                let strike = (*strike as f64 / FLOATING_TO_FIXED_OFF);
                buffer[idx..idx + 8].copy_from_slice(&strike.to_le_bytes()[..]);
                buffer[idx + 8..idx + 16].copy_from_slice(&volume.to_le_bytes()[..]);
                idx += 16;

                if idx >= 0x800 {
                    break;
                }
            }

            idx = 0x800;
            buffer[idx] = 0xc2;

            for (strike, lvl2) in order_book.asks.iter_mut() {
                let volume: isize = lvl2
                    .iter_mut()
                    .map(|entry| {
                        entry.cycles_present += 1;
                        entry.amount
                    })
                    .sum();
                let strike = (*strike as f64 / FLOATING_TO_FIXED_OFF);
                buffer[idx..idx + 8].copy_from_slice(&strike.to_le_bytes()[..]);
                buffer[idx + 8..idx + 16].copy_from_slice(&strike.to_le_bytes()[..]);
                idx += 16;
                if idx >= buffer.len() {
                    break;
                }
            }

            for (addr, client) in lock.iter_mut() {
                client.cycles_present += 1;
                socket.send_to(&buffer, addr);
                socket.send_to(&client.to_bytes(), addr);
            }
        }
    }
}

// TODO: SENT OUT NBBO/LVL2
 
