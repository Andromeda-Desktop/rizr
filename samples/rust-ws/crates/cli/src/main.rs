fn main() {
    println!("{}", ws_core::add(1, 2));
}

#[cfg(test)]
mod tests {
    #[test]
    fn uses_core() {
        assert_eq!(ws_core::add(1, 2), 3);
    }
}
