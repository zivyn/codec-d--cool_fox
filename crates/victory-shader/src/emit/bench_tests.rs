//! A timing run of one console compute container on the host card, with synthetic buffers of
//! the sizes the environment names (in the card's memory or the PC's, to see what the placement
//! of a buffer costs). Skipped unless `VICTORY_BENCH_CONTAINER` names a container file (nothing
//! of any title is embedded); it prints the milliseconds per dispatch and how many dwords the
//! program wrote in each UAV, which says whether the dispatch did its work:
//!
//! - `VICTORY_BENCH_GROUPS=x,y,z`: the dispatch (default 1,1,1)
//! - `VICTORY_BENCH_VIEWS=index:u|s:dwords,...`: the raw UAVs (`u`) and SRVs (`s`) of the view
//!   heap by index, in the PC's memory when written in capitals (`U`, `S`)
//! - `VICTORY_BENCH_USER=index=value,...`: user-data dwords (the others 0)
//! - `VICTORY_BENCH_CBV=dword=value,...`: dwords of the first root CBV (the others 0)
//! - `VICTORY_BENCH_REPS=n`: timed dispatches (default 10)
//! - `VICTORY_BENCH_WAVE=n`: the host's wave size to translate for (default 32)
//! - `VICTORY_BENCH_DUMP=path`: also writes the emitted DXIL there

use super::host::{BenchView, Host};
use super::lower;
use crate::dxbc::{Container, FourCC};
use crate::{io, native, resource, spill, structure};

fn pairs(var: &str, sep: char) -> Vec<(u32, u32)> {
    std::env::var(var)
        .unwrap_or_default()
        .split(',')
        .filter_map(|p| {
            let (a, b) = p.split_once(sep)?;
            Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
        })
        .collect()
}

#[test]
fn a_container_is_timed_on_the_card() {
    let Some(path) = std::env::var_os("VICTORY_BENCH_CONTAINER") else {
        eprintln!("VICTORY_BENCH_CONTAINER is not set; skipped");
        return;
    };
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let bytes = std::fs::read(path).expect("the container");
    let container = Container::parse(&bytes).expect("a container");
    let part = container
        .part(FourCC::DXIL)
        .or(container.part(FourCC::ACCC))
        .expect("a program part")
        .data;
    let wave = std::env::var("VICTORY_BENCH_WAVE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(32);
    let was = lower::TEST_HOST_WAVE.replace(Some(wave));
    let shaders: Vec<_> = native::native_programs(part)
        .expect("programs")
        .iter()
        .map(|n| {
            let p = spill::build(n).expect("spill::build");
            let st = structure::structure(&p).expect("structure");
            let res = resource::track(&p, &n.layout, n.stage, Some(&st)).expect("resource::track");
            let io = io::raise(&p, n).expect("io::raise");
            (
                super::emit(&container, n, &p, &st, &io, &res).expect("emit"),
                n.wave64,
            )
        })
        .collect();
    lower::TEST_HOST_WAVE.set(was);
    let (shader, wave64) = shaders.into_iter().next().expect("a program");
    if let Some(out) = std::env::var_os("VICTORY_BENCH_DUMP") {
        std::fs::write(out, &shader.dxil).expect("the dump");
    }
    eprintln!(
        "wave64 {wave64}, dxil {} bytes, user data {:?}, root cbvs {:?} srvs {:?} uavs {:?}",
        shader.dxil.len(),
        shader.user_data,
        shader.root_cbvs,
        shader.root_srvs,
        shader.root_uavs
    );
    let host = Host::new(false).expect("a hardware device");
    let pipeline = host.compute_pipeline(&shader).expect("compute_pipeline");
    let mut user = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for (i, v) in pairs("VICTORY_BENCH_USER", '=') {
        if let Some(d) = user.get_mut(i as usize) {
            *d = v;
        }
    }
    let mut cbv = vec![0u32; 16384];
    for (i, v) in pairs("VICTORY_BENCH_CBV", '=') {
        cbv[i as usize] = v;
    }
    let cbvs: Vec<Vec<u32>> = (0..shader.root_cbvs.len()).map(|_| cbv.clone()).collect();
    let views: Vec<BenchView> = std::env::var("VICTORY_BENCH_VIEWS")
        .unwrap_or_default()
        .split(',')
        .filter_map(|v| {
            let mut f = v.split(':');
            let index = f.next()?.trim().parse().ok()?;
            let kind = f.next()?.trim();
            Some(BenchView {
                index,
                uav: kind.eq_ignore_ascii_case("u"),
                sysmem: kind.chars().all(|c| c.is_ascii_uppercase()),
                dwords: f.next()?.trim().parse().ok()?,
            })
        })
        .collect();
    let g: Vec<u32> = std::env::var("VICTORY_BENCH_GROUPS")
        .unwrap_or_else(|_| "1,1,1".into())
        .split(',')
        .filter_map(|v| v.trim().parse().ok())
        .collect();
    let groups = [
        g[0],
        g.get(1).copied().unwrap_or(1),
        g.get(2).copied().unwrap_or(1),
    ];
    let reps = std::env::var("VICTORY_BENCH_REPS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(10);
    let (ms, written) = host
        .time_compute(&pipeline, &user, &cbvs, &views, groups, reps)
        .expect("time_compute");
    eprintln!(
        "BENCH {ms:.3} ms per dispatch of {groups:?}; dwords written in each UAV: {written:?}"
    );
}
