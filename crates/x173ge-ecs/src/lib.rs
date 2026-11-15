//! Shipyard integration; storage and scheduling remain owned by Shipyard.

pub use shipyard::{
    AddEntity, Component, EntitiesView, EntitiesViewMut, Get, IntoIter, IntoWorkload, Unique,
    UniqueView, UniqueViewMut, View, ViewMut, Workload, World,
};
