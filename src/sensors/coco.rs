//! The label set the object detectors share (origin `detectors.json`, set `coco`) and the choice of
//! detector. Both sides of the wire name a class by its key; the index is the model's class order.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::types::InvalidInput;

/// One class of the `coco` set: the key the origin stores, the name shown, the group it is listed in.
pub struct ClassInfo {
    pub key: &'static str,
    pub name: &'static str,
    pub group: &'static str,
}

const fn class(key: &'static str, name: &'static str, group: &'static str) -> ClassInfo {
    ClassInfo { key, name, group }
}

/// In the model's class order, as in `detectors.json`.
pub const COCO: [ClassInfo; 80] = [
    class("person", "Pessoa", "Pessoas"),
    class("bicycle", "Bicicleta", "Veículos"),
    class("car", "Carro", "Veículos"),
    class("motorcycle", "Moto", "Veículos"),
    class("airplane", "Avião", "Veículos"),
    class("bus", "Ônibus", "Veículos"),
    class("train", "Trem", "Veículos"),
    class("truck", "Caminhão", "Veículos"),
    class("boat", "Barco", "Veículos"),
    class("traffic_light", "Semáforo", "Rua"),
    class("fire_hydrant", "Hidrante", "Rua"),
    class("stop_sign", "Placa de pare", "Rua"),
    class("parking_meter", "Parquímetro", "Rua"),
    class("bench", "Banco de praça", "Rua"),
    class("bird", "Pássaro", "Animais"),
    class("cat", "Gato", "Animais"),
    class("dog", "Cachorro", "Animais"),
    class("horse", "Cavalo", "Animais"),
    class("sheep", "Ovelha", "Animais"),
    class("cow", "Vaca", "Animais"),
    class("elephant", "Elefante", "Animais"),
    class("bear", "Urso", "Animais"),
    class("zebra", "Zebra", "Animais"),
    class("giraffe", "Girafa", "Animais"),
    class("backpack", "Mochila", "Acessórios"),
    class("umbrella", "Guarda-chuva", "Acessórios"),
    class("handbag", "Bolsa", "Acessórios"),
    class("tie", "Gravata", "Acessórios"),
    class("suitcase", "Mala", "Acessórios"),
    class("frisbee", "Frisbee", "Esportes"),
    class("skis", "Esquis", "Esportes"),
    class("snowboard", "Snowboard", "Esportes"),
    class("sports_ball", "Bola", "Esportes"),
    class("kite", "Pipa", "Esportes"),
    class("baseball_bat", "Taco de beisebol", "Esportes"),
    class("baseball_glove", "Luva de beisebol", "Esportes"),
    class("skateboard", "Skate", "Esportes"),
    class("surfboard", "Prancha de surfe", "Esportes"),
    class("tennis_racket", "Raquete de tênis", "Esportes"),
    class("bottle", "Garrafa", "Cozinha"),
    class("wine_glass", "Taça", "Cozinha"),
    class("cup", "Xícara", "Cozinha"),
    class("fork", "Garfo", "Cozinha"),
    class("knife", "Faca", "Cozinha"),
    class("spoon", "Colher", "Cozinha"),
    class("bowl", "Tigela", "Cozinha"),
    class("banana", "Banana", "Comida"),
    class("apple", "Maçã", "Comida"),
    class("sandwich", "Sanduíche", "Comida"),
    class("orange", "Laranja", "Comida"),
    class("broccoli", "Brócolis", "Comida"),
    class("carrot", "Cenoura", "Comida"),
    class("hot_dog", "Cachorro-quente", "Comida"),
    class("pizza", "Pizza", "Comida"),
    class("donut", "Rosquinha", "Comida"),
    class("cake", "Bolo", "Comida"),
    class("chair", "Cadeira", "Móveis"),
    class("couch", "Sofá", "Móveis"),
    class("potted_plant", "Vaso de planta", "Móveis"),
    class("bed", "Cama", "Móveis"),
    class("dining_table", "Mesa", "Móveis"),
    class("toilet", "Vaso sanitário", "Móveis"),
    class("tv", "TV", "Eletrônicos"),
    class("laptop", "Notebook", "Eletrônicos"),
    class("mouse", "Mouse", "Eletrônicos"),
    class("remote", "Controle remoto", "Eletrônicos"),
    class("keyboard", "Teclado", "Eletrônicos"),
    class("cell_phone", "Celular", "Eletrônicos"),
    class("microwave", "Micro-ondas", "Eletrodomésticos"),
    class("oven", "Forno", "Eletrodomésticos"),
    class("toaster", "Torradeira", "Eletrodomésticos"),
    class("sink", "Pia", "Eletrodomésticos"),
    class("refrigerator", "Geladeira", "Eletrodomésticos"),
    class("book", "Livro", "Casa"),
    class("clock", "Relógio", "Casa"),
    class("vase", "Vaso", "Casa"),
    class("scissors", "Tesoura", "Casa"),
    class("teddy_bear", "Ursinho de pelúcia", "Casa"),
    class("hair_drier", "Secador de cabelo", "Casa"),
    class("toothbrush", "Escova de dentes", "Casa"),
];

/// Offered first when choosing classes (`detectors.json`, `popular`).
pub const POPULAR: [&str; 8] = ["person", "car", "cat", "dog", "bicycle", "truck", "motorcycle", "bird"];

/// A class of the `coco` set. On the wire and in the database it is its key, checked on the way in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectClass(u8);

impl ObjectClass {
    /// `person`, the first class of the set: its boxes are where faces are looked for.
    pub const PERSON: Self = Self(0);

    /// The class at this position of the model's output, if the set has one.
    pub fn from_index(index: usize) -> Option<Self> {
        (index < COCO.len()).then_some(Self(index as u8))
    }

    pub fn index(self) -> usize {
        self.0.into()
    }

    pub fn key(self) -> &'static str {
        COCO[self.index()].key
    }

    pub fn name(self) -> &'static str {
        COCO[self.index()].name
    }

    pub fn group(self) -> &'static str {
        COCO[self.index()].group
    }

    pub fn all() -> impl Iterator<Item = Self> {
        (0..COCO.len()).map(|i| Self(i as u8))
    }

    pub fn by_key(key: &str) -> Option<Self> {
        COCO.iter().position(|c| c.key == key).and_then(Self::from_index)
    }
}

impl TryFrom<String> for ObjectClass {
    type Error = InvalidInput;
    fn try_from(key: String) -> Result<Self, InvalidInput> {
        Self::by_key(&key).ok_or(InvalidInput("objeto desconhecido"))
    }
}

impl From<ObjectClass> for String {
    fn from(class: ObjectClass) -> String {
        class.key().to_owned()
    }
}

impl fmt::Display for ObjectClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Which object detector runs for every object sensor (origin `detectors.json`; one global
/// choice, as the origin's `detector` setting). All of them read the `coco` set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Detector {
    #[serde(rename = "dfine-n-coco")]
    DfineN,
    #[default]
    #[serde(rename = "dfine-s-coco")]
    DfineS,
    #[serde(rename = "dfine-m-coco")]
    DfineM,
    #[serde(rename = "dfine-l-coco")]
    DfineL,
}

impl Detector {
    pub const ALL: [Self; 4] = [Self::DfineN, Self::DfineS, Self::DfineM, Self::DfineL];

    /// The origin's id, also the model's id in the registry.
    pub const fn id(self) -> &'static str {
        match self {
            Self::DfineN => "dfine-n-coco",
            Self::DfineS => "dfine-s-coco",
            Self::DfineM => "dfine-m-coco",
            Self::DfineL => "dfine-l-coco",
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::DfineN => "D-FINE N (rápido)",
            Self::DfineS => "D-FINE S (equilibrado)",
            Self::DfineM => "D-FINE M (preciso)",
            Self::DfineL => "D-FINE L (mais preciso)",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::DfineN => "Cerca de duas vezes mais rápido e mais leve, mas deixa passar mais objetos. 15 MB.",
            Self::DfineS => "Recomendado. Acha objetos pequenos e parcialmente escondidos; cerca de 0,1 s por checagem. 42 MB.",
            Self::DfineM => "Acha mais objetos pequenos e distantes que o S, mas é umas três vezes mais lento. 79 MB.",
            Self::DfineL => "O mais preciso, umas quatro vezes mais lento que o S. 125 MB.",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_label_set_is_complete_and_its_keys_round_trip() {
        let keys: std::collections::BTreeSet<&str> = COCO.iter().map(|c| c.key).collect();
        assert_eq!(keys.len(), 80, "80 distinct classes");
        assert!(POPULAR.iter().all(|k| keys.contains(k)));
        assert_eq!(ObjectClass::by_key("car").map(ObjectClass::index), Some(2), "the model's class order");
        let car: ObjectClass = serde_json::from_str(r#""car""#).unwrap();
        assert_eq!((serde_json::to_string(&car).unwrap().as_str(), car.name()), (r#""car""#, "Carro"));
        assert!(serde_json::from_str::<ObjectClass>(r#""unicorn""#).is_err());
        assert_eq!(ObjectClass::from_index(80), None);
    }

    #[test]
    fn the_detector_choice_keeps_the_origin_ids() {
        assert_eq!(Detector::default(), Detector::DfineS);
        assert_eq!(serde_json::to_string(&Detector::DfineM).unwrap(), r#""dfine-m-coco""#);
    }
}
