use pretty_assertions::assert_eq;

use super::resume_successor_created_at;

#[test]
fn resume_successor_created_at_is_stable_for_a_claimed_uuid_v7() {
    let run_id = uuid::Uuid::now_v7().to_string();

    let first = resume_successor_created_at(&run_id).expect("derive successor timestamp");
    let reconstructed =
        resume_successor_created_at(&run_id).expect("reconstruct successor timestamp");

    assert_eq!(reconstructed, first);
}
