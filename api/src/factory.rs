use kube::core::object::HasSpec;
use rand::seq::IndexedRandom;

fn calc_word_num(mut bits: usize) -> usize {
    const NOUNS_LEN: u128 = names::NOUNS.len() as u128;
    const ADJECTIVES_LEN: u128 = names::ADJECTIVES.len() as u128;
    let mut target = 1u128;
    while bits > 1 {
        target <<= 1;
        target |= 1;
        bits -= 1;
    }
    target /= NOUNS_LEN;
    let mut len = 1;
    while target > 0 {
        target /= ADJECTIVES_LEN;
        len += 1;
    }
    len
}

const NAME_BITS: usize = u32::BITS as usize;
lazy_static::lazy_static! {
    static ref NAME_LEN: usize = calc_word_num(NAME_BITS);
}

fn gen_name(rng: &mut impl rand::Rng, len: usize) -> String {
    let noun = names::NOUNS.choose(rng).unwrap();
    (1..len)
        .map(|_| names::ADJECTIVES.choose(rng).unwrap())
        .chain(std::iter::once(noun))
        .copied()
        .collect::<Vec<_>>()
        .join("-")
}

pub fn rand_name() -> String {
    format!("kubimo-{}", gen_name(&mut rand::rng(), *NAME_LEN))
}

pub trait ResourceFactory: HasSpec + Sized {
    fn new(name: &str, spec: Self::Spec) -> Self;
}

pub trait ResourceFactoryExt: ResourceFactory {
    fn create(spec: Self::Spec) -> Self {
        Self::new(&rand_name(), spec)
    }
}

impl<T> ResourceFactoryExt for T where T: ResourceFactory {}
