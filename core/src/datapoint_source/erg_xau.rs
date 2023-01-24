//! Obtains the nanoErg per 1 XAU (troy ounce of gold) rate

use super::aggregator::DataPointSourceAggregator;
use super::assets_exchange_rate::Asset;
use super::assets_exchange_rate::NanoErg;
use super::coingecko;
use super::combined::BitPandaViaCoinCap;

pub struct KgAu {}
pub struct Xau {}

impl Asset for KgAu {}
impl Asset for Xau {}

impl KgAu {
    pub fn from_xau(xau: f64) -> f64 {
        // https://en.wikipedia.org/wiki/Gold_bar
        // troy ounces per kg
        xau * 32.150746568627
    }
}

pub fn kgau_nanoerg_aggregator() -> Box<DataPointSourceAggregator<KgAu, NanoErg>> {
    Box::new(DataPointSourceAggregator::<KgAu, NanoErg> {
        fetchers: vec![Box::new(coingecko::CoinGecko), Box::new(BitPandaViaCoinCap)],
    })
}

#[cfg(test)]
mod tests {
    use crate::datapoint_source::DataPointSource;

    use super::*;

    #[test]
    fn test_aggegator() {
        let n = kgau_nanoerg_aggregator();
        let rate = n.get_datapoint().unwrap();
        assert!(rate > 0);
    }
}
