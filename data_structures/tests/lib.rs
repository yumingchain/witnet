extern crate witnet_data_structures as data_structures;

use data_structures::greetings;

#[test]
fn data_structures_greeeting() {
    assert_eq!(greetings(), String::from("Hello form data structures!"));
}
