//! Cross-references using anchor keys: single-to-single, single-to-many,
//! many-to-many.
//!
//! # Why anchor keys?
//!
//! Every `Id` embeds a `CREATED_AT` timestamp that pins it to one record
//! version. Storing a raw `Id` as a cross-reference binds you to that version:
//! if the record is ever replaced at the same logical slot (tombstone +
//! re-create), the reference becomes stale and resolving it requires walking
//! the version chain in `Metadata`.
//!
//! `id.anchor_key()` strips `CREATED_AT` to zero, leaving only
//! `TENANT_ID | SHARD_ID | STRUCT_ID`. The storage engine maps this **stable
//! anchor address** to whatever live version currently occupies that slot.
//! Storing anchor keys means:
//!
//! - references survive mutations and full record replacement,
//! - no history walk is needed to find the current version,
//! - the identity a cross-reference cares about never changes.
//!
//! # Write / read pattern
//!
//! ```text
//! write: ref_field = other.id.anchor_key()
//! read:  candidates.iter().find(|r| r.id.anchor_key() == stored_anchor)
//! ```
//!
//! # Cardinalities
//!
//! | Pattern | Description | Example |
//! |---------|-------------|---------|
//! | **1 → 1** | Both records store each other's anchor key | Citizen ↔ Passport |
//! | **1 → N** | Each child stores the parent's anchor key | Company → Employees |
//! | **M → N** | Junction record holds two anchor keys | Student ↔ Course |
//!
//! Run with:
//!   cargo run --bin cross_references

use wavedb::prelude::*;
use wavedb_net::MockTransport;
use wavedb_net::mock::ScriptedReply;

// ── Schema ───────────────────────────────────────────────────────────────────

// ── 1. Single-to-Single (1:1) — Citizen ↔ Passport ──────────────────────────

#[wave_db(struct_id = 60, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Citizen1 {
    pub name: String,
    /// Typed anchor of this citizen's `Passport`.
    pub passport_anchor: PassportAnchor,
}
pub type Citizen = Citizen1;
pub type CitizenAnchor = Citizen1Anchor;

#[wave_db(struct_id = 61, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Passport1 {
    pub number: String,
    /// Typed anchor of the owning `Citizen`.
    pub citizen_anchor: CitizenAnchor,
}
pub type Passport = Passport1;
pub type PassportAnchor = Passport1Anchor;

// ── 2. Single-to-Many (1:N) — Company → Employees ────────────────────────────

#[wave_db(struct_id = 62, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Company1 {
    pub name: String,
}
pub type Company = Company1;
pub type CompanyAnchor = Company1Anchor;

#[wave_db(struct_id = 63, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Worker1 {
    pub name: String,
    /// Typed anchor of the `Company` this worker belongs to.
    pub company_anchor: CompanyAnchor,
}
pub type Worker = Worker1;

// ── 3. Many-to-Many (M:N) — Student ↔ Course via junction ────────────────────

#[wave_db(struct_id = 64, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Student1 {
    pub name: String,
}
pub type Student = Student1;
pub type StudentAnchor = Student1Anchor;

#[wave_db(struct_id = 65, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Course1 {
    pub title: String,
}
pub type Course = Course1;
pub type CourseAnchor = Course1Anchor;

/// Junction record for the M:N Student ↔ Course relationship.
///
/// Both fields are anchor keys so either side can be updated or replaced
/// without invalidating existing enrollments.
#[wave_db(struct_id = 66, NonUnique)]
#[derive(PartialEq, Eq)]
pub struct Enrollment1 {
    pub student_anchor: StudentAnchor,
    pub course_anchor: CourseAnchor,
}
pub type Enrollment = Enrollment1;

// ── Wire helper ──────────────────────────────────────────────────────────────

fn encode_query<T>(records: &[T]) -> Vec<u8>
where
    T: serde::Serialize + wavedb_core::WaveDbStruct,
{
    let entries: Vec<(u8, Vec<u8>)> = records
        .iter()
        .map(|r| (T::STRUCT_VERSION, postcard::to_allocvec(r).unwrap()))
        .collect();
    postcard::to_allocvec(&entries).unwrap()
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let tenant = 42u64;

    // ── 1. Single-to-Single data ─────────────────────────────────────────────
    //
    // citizen_v1 and citizen_v2 share SHARD = 1 but differ in CREATED_AT.
    // This simulates a "record replaced at the same anchor" scenario: same
    // logical slot, new version timestamp. The passport stores the anchor
    // key (CREATED_AT = 0) so it resolves correctly to whichever version
    // is currently live — no history walk required.

    let citizen_v1_id = Id::new(tenant, 1, Citizen::STRUCT_ID, 1_000);
    let citizen_v2_id = Id::new(tenant, 1, Citizen::STRUCT_ID, 2_000); // same SHARD, new CREATED_AT
    let passport_id = Id::new(tenant, 2, Passport::STRUCT_ID, 1_001);

    // `From<Id> for FooAnchor` always calls `anchor_key()`, enforcing CREATED_AT = 0.
    let citizen_anchor = Citizen1Anchor::from(citizen_v1_id); // == from(citizen_v2_id)
    let passport_anchor = Passport1Anchor::from(passport_id);

    let citizen_v1 = Citizen {
        id: citizen_v1_id,
        name: "Alice".into(),
        passport_anchor,
        ..Default::default()
    };
    let passport = Passport {
        id: passport_id,
        number: "XY-9001".into(),
        citizen_anchor, // typed: stable across v1 → v2 replacement
        ..Default::default()
    };
    // v2: replaced at the same anchor (SHARD = 1), updated name.
    let citizen_v2 = Citizen {
        id: citizen_v2_id,
        name: "Alice Liddell".into(),
        passport_anchor,
        ..Default::default()
    };

    // Typed anchors are equal despite different CREATED_AT — the compiler
    // enforces this invariant via From<Id> calling anchor_key().
    assert_eq!(
        Citizen1Anchor::from(citizen_v1_id),
        Citizen1Anchor::from(citizen_v2_id)
    );
    assert_eq!(passport.citizen_anchor, Citizen1Anchor::from(citizen_v2_id));

    // ── 2. Single-to-Many data ───────────────────────────────────────────────

    let company_id = Id::new(tenant, 3, Company::STRUCT_ID, 2_000);
    let company = Company {
        id: company_id,
        name: "Acme Corp".into(),
        ..Default::default()
    };
    let worker_alice = Worker {
        name: "Alice".into(),
        company_anchor: Company1Anchor::from(company_id),
        ..Default::default()
    };
    let worker_bob = Worker {
        name: "Bob".into(),
        company_anchor: Company1Anchor::from(company_id),
        ..Default::default()
    };

    // ── 3. Many-to-Many data ─────────────────────────────────────────────────

    let student_alice_id = Id::new(tenant, 4, Student::STRUCT_ID, 3_000);
    let student_bob_id = Id::new(tenant, 5, Student::STRUCT_ID, 3_001);
    let course_rust_id = Id::new(tenant, 6, Course::STRUCT_ID, 4_000);
    let course_async_id = Id::new(tenant, 7, Course::STRUCT_ID, 4_001);

    let student_alice = Student {
        id: student_alice_id,
        name: "Alice".into(),
        ..Default::default()
    };
    let student_bob = Student {
        id: student_bob_id,
        name: "Bob".into(),
        ..Default::default()
    };
    let course_rust = Course {
        id: course_rust_id,
        title: "Rust".into(),
        ..Default::default()
    };
    let course_async = Course {
        id: course_async_id,
        title: "Async".into(),
        ..Default::default()
    };

    // Alice enrolls in Rust and Async; Bob enrolls only in Rust.
    // Typed anchors prevent accidentally swapping student_anchor / course_anchor.
    let enroll_alice_rust = Enrollment {
        student_anchor: Student1Anchor::from(student_alice_id),
        course_anchor: Course1Anchor::from(course_rust_id),
        ..Default::default()
    };
    let enroll_alice_async = Enrollment {
        student_anchor: Student1Anchor::from(student_alice_id),
        course_anchor: Course1Anchor::from(course_async_id),
        ..Default::default()
    };
    let enroll_bob_rust = Enrollment {
        student_anchor: Student1Anchor::from(student_bob_id),
        course_anchor: Course1Anchor::from(course_rust_id),
        ..Default::default()
    };

    // ── Scripted server replies ──────────────────────────────────────────────

    let mock = MockTransport::new();
    mock.push(ScriptedReply::connect(
        "ws://owner:7700",
        "ws://backup:7700",
    ));

    // 1:1 — write citizen_v1, passport, citizen_v2 (replacement at same anchor)
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    // 1:1 — query: engine returns the live (v2) citizen
    mock.push(ScriptedReply::ok(encode_query(std::slice::from_ref(
        &citizen_v2,
    ))));

    // 1:N — write company, worker_alice, worker_bob
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    // 1:N — query: all workers
    mock.push(ScriptedReply::ok(encode_query(&[
        worker_alice.clone(),
        worker_bob.clone(),
    ])));

    // M:N — write 2 students + 2 courses + 3 enrollments
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    mock.push(ScriptedReply::ok(Vec::new()));
    // M:N — queries: all enrollments, then courses, then students
    mock.push(ScriptedReply::ok(encode_query(&[
        enroll_alice_rust.clone(),
        enroll_alice_async.clone(),
        enroll_bob_rust.clone(),
    ])));
    mock.push(ScriptedReply::ok(encode_query(&[
        course_rust.clone(),
        course_async.clone(),
    ])));
    mock.push(ScriptedReply::ok(encode_query(&[
        student_alice.clone(),
        student_bob.clone(),
    ])));

    let db = Db::open_with_transport(mock, /* user= */ 1, tenant).await?;

    // ── 1. Single-to-Single operations ───────────────────────────────────────

    println!("=== 1. Single → Single (Citizen ↔ Passport) ===");
    citizen_v1.clone().save(&db).await?;
    passport.clone().save(&db).await?;
    citizen_v2.clone().save(&db).await?; // replacement at same anchor (SHARD = 1)
    println!("Wrote citizen_v1, passport, citizen_v2 (replacement at same anchor)");

    // Query returns the live citizen (v2). Follow the reference by comparing
    // anchor keys — CREATED_AT is irrelevant so the resolution is stable.
    let citizens = Citizen::query(&db, Expr::all()).await?;
    assert_eq!(citizens.len(), 1);
    let live_citizen = &citizens[0];
    assert_eq!(live_citizen.name, "Alice Liddell"); // v2 name

    // Passport's citizen_anchor resolves to v2 — typed comparison enforces kind.
    assert_eq!(live_citizen.anchor(), passport.citizen_anchor);
    // Citizen's passport_anchor resolves to the passport.
    assert_eq!(
        citizen_v1.passport_anchor,
        Passport1Anchor::from(passport_id)
    );

    println!(
        "Passport {}: citizen_anchor → {:?} (stable across replacement)",
        passport.number, live_citizen.name
    );

    // ── 2. Single-to-Many operations ─────────────────────────────────────────

    println!();
    println!("=== 2. Single → Many (Company → Workers) ===");
    company.clone().save(&db).await?;
    worker_alice.clone().save(&db).await?;
    worker_bob.clone().save(&db).await?;
    println!("Wrote 1 company, 2 workers");

    // Resolve: all workers whose typed company_anchor matches.
    let workers = Worker::query(&db, Expr::all()).await?;
    let acme_workers: Vec<&Worker> = workers
        .iter()
        .filter(|w| w.company_anchor == Company1Anchor::from(company_id))
        .collect();
    assert_eq!(acme_workers.len(), 2);
    println!(
        "Workers at {:?}: {}",
        company.name,
        acme_workers
            .iter()
            .map(|w| w.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    // ── 3. Many-to-Many operations ────────────────────────────────────────────

    println!();
    println!("=== 3. Many → Many (Student ↔ Course via Enrollment) ===");
    student_alice.clone().save(&db).await?;
    student_bob.clone().save(&db).await?;
    course_rust.clone().save(&db).await?;
    course_async.clone().save(&db).await?;
    enroll_alice_rust.clone().save(&db).await?;
    enroll_alice_async.clone().save(&db).await?;
    enroll_bob_rust.clone().save(&db).await?;
    println!("Wrote 2 students, 2 courses, 3 enrollments");

    let all_enrollments = Enrollment::query(&db, Expr::all()).await?;

    // Alice's courses — step 1: collect typed Course1Anchors from her enrollments.
    // Typed vectors prevent mixing student_anchors and course_anchors accidentally.
    let alice_anchor = Student1Anchor::from(student_alice_id);
    let alice_course_anchors: Vec<Course1Anchor> = all_enrollments
        .iter()
        .filter(|e| e.student_anchor == alice_anchor)
        .map(|e| e.course_anchor)
        .collect();

    // Step 2: fetch courses and filter — `.anchor()` returns Course1Anchor.
    let all_courses = Course::query(&db, Expr::all()).await?;
    let alice_courses: Vec<&Course> = all_courses
        .iter()
        .filter(|c| alice_course_anchors.contains(&c.anchor()))
        .collect();
    assert_eq!(alice_courses.len(), 2);

    // Rust course's students — same two-step pattern in reverse.
    let rust_anchor = Course1Anchor::from(course_rust_id);
    let rust_student_anchors: Vec<Student1Anchor> = all_enrollments
        .iter()
        .filter(|e| e.course_anchor == rust_anchor)
        .map(|e| e.student_anchor)
        .collect();

    let all_students = Student::query(&db, Expr::all()).await?;
    let rust_students: Vec<&Student> = all_students
        .iter()
        .filter(|s| rust_student_anchors.contains(&s.anchor()))
        .collect();
    assert_eq!(rust_students.len(), 2);

    println!(
        "Alice's courses: {}",
        alice_courses
            .iter()
            .map(|c| c.title.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "Students in {:?}: {}",
        course_rust.title,
        rust_students
            .iter()
            .map(|s| s.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );

    println!();
    println!("cross_references example OK");
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_main() {
        super::main().unwrap();
    }
}
