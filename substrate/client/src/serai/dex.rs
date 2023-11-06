use sp_core::bounded_vec::BoundedVec;
use serai_runtime::{
  primitives::{SeraiAddress, Amount, Coin},
  dex, Dex, Runtime,
};

use subxt::tx::Payload;

use crate::{SeraiError, Composite, TemporalSerai, scale_composite};

const PALLET: &str = "Dex";

pub type DexEvent = dex::Event<Runtime>;

#[derive(Clone, Copy)]
pub struct SeraiDex<'a>(pub(crate) TemporalSerai<'a>);
impl<'a> SeraiDex<'a> {
  pub async fn all_events(&self) -> Result<Vec<DexEvent>, SeraiError> {
    self.0.events::<Dex, _>(|_| true).await
  }

  pub fn add_liquidity(
    coin: Coin,
    coin_amount: Amount,
    sri_amount: Amount,
    min_coin_amount: Amount,
    min_sri_amount: Amount,
    address: SeraiAddress,
  ) -> Payload<Composite<()>> {
    Payload::new(
      PALLET,
      "add_liquidity",
      scale_composite(dex::Call::<Runtime>::add_liquidity {
        coin1: coin,
        coin2: Coin::Serai,
        amount1_desired: coin_amount.0,
        amount2_desired: sri_amount.0,
        amount1_min: min_coin_amount.0,
        amount2_min: min_sri_amount.0,
        mint_to: address.into(),
      }),
    )
  }

  pub fn swap(
    from_coin: Coin,
    to_coin: Coin,
    amount_in: Amount,
    amount_out_min: Amount,
    address: SeraiAddress,
  ) -> Payload<Composite<()>> {
    let path = if to_coin.is_native() {
      BoundedVec::try_from(vec![from_coin, Coin::Serai]).unwrap()
    } else if from_coin.is_native() {
      BoundedVec::try_from(vec![Coin::Serai, to_coin]).unwrap()
    } else {
      BoundedVec::try_from(vec![from_coin, Coin::Serai, to_coin]).unwrap()
    };

    Payload::new(
      PALLET,
      "swap_exact_tokens_for_tokens",
      scale_composite(dex::Call::<Runtime>::swap_exact_tokens_for_tokens {
        path,
        amount_in: amount_in.0,
        amount_out_min: amount_out_min.0,
        send_to: address.into(),
      }),
    )
  }
}