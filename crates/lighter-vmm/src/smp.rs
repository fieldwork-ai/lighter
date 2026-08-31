//! Secondary CPU bring-up.
//!
//! PSCI `CPU_ON` arrives on whichever vCPU thread happened to make the call,
//! and has to start a *different* core. Since a vCPU is bound to its own
//! thread, that thread must already exist — so every core gets a thread at boot
//! and secondaries park here until the guest asks for them.
//!
//! Parking rather than spawning on demand also keeps thread creation out of the
//! hypercall path, where it would be a syscall storm inside a fault handler.

use std::sync::{Condvar, Mutex};

use crate::psci::PsciReturn;

/// Where a secondary core should start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StartRequest {
    pub entry_point: u64,
    pub context_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuState {
    Off,
    /// Powered on with a pending start request the core has not consumed yet.
    Starting(StartRequest),
    On,
}

struct Cpu {
    state: Mutex<CpuState>,
    wake: Condvar,
}

/// The power state of every core in the machine.
pub struct CpuPark {
    cpus: Vec<Cpu>,
    /// Set when the machine is going away, so parked cores stop waiting.
    shutdown: Mutex<bool>,
    /// How many cores have created their vCPU so far.
    ///
    /// See [`CpuPark::await_creation_turn`] for why this exists.
    created: Mutex<u32>,
    creation_turn: Condvar,
}

impl CpuPark {
    /// Creates the park for `count` cores, with core 0 already running.
    pub fn new(count: u32) -> CpuPark {
        let cpus = (0..count)
            .map(|index| Cpu {
                state: Mutex::new(if index == 0 {
                    CpuState::On
                } else {
                    CpuState::Off
                }),
                wake: Condvar::new(),
            })
            .collect();
        CpuPark {
            cpus,
            shutdown: Mutex::new(false),
            created: Mutex::new(0),
            creation_turn: Condvar::new(),
        }
    }

    /// Blocks until it is core `index`'s turn to create its vCPU.
    ///
    /// # Why creation must be ordered
    ///
    /// `hv_vcpu_create` hands out ids in call order, and Apple's GIC gives
    /// vCPU *id* N the Nth redistributor. Meanwhile the device tree says the
    /// core whose `MPIDR` affinity is N uses that same Nth redistributor. So
    /// the three numbers — thread index, vCPU id, and MPIDR — all have to
    /// agree.
    ///
    /// Left to race, they do not: whichever thread reaches `create_vcpu` first
    /// gets id 0, and it is not reliably thread 0. The core the kernel calls
    /// CPU0 then owns a redistributor the device tree assigned to a different
    /// core, and the guest dies during `init_IRQ` with "GICv3: No
    /// redistributor present" — intermittently, and only with more than one
    /// core, which is exactly how this was found.
    ///
    /// Serializing creation costs one handshake per core at boot and makes the
    /// invariant hold by construction.
    pub fn await_creation_turn(&self, index: u32) {
        let mut created = self.created.lock().expect("creation counter poisoned");
        while *created != index {
            created = self
                .creation_turn
                .wait(created)
                .expect("creation counter poisoned");
        }
    }

    /// Records that core `index` has created its vCPU, releasing the next one.
    pub fn finish_creation(&self, index: u32) {
        let mut created = self.created.lock().expect("creation counter poisoned");
        debug_assert_eq!(*created, index, "cores created out of order");
        *created = index + 1;
        self.creation_turn.notify_all();
    }

    pub fn len(&self) -> u32 {
        self.cpus.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.cpus.is_empty()
    }

    /// Handles PSCI `CPU_ON` for `index`.
    ///
    /// The return value is the guest's, verbatim: `ALREADY_ON` and
    /// `INVALID_PARAMS` are both states Linux probes for deliberately during
    /// hotplug, so neither is an error on our side.
    pub fn power_on(&self, index: u32, request: StartRequest) -> PsciReturn {
        let Some(cpu) = self.cpus.get(index as usize) else {
            return PsciReturn::InvalidParams;
        };
        let mut state = cpu.state.lock().expect("cpu state poisoned");
        match *state {
            CpuState::On => PsciReturn::AlreadyOn,
            CpuState::Starting(_) => PsciReturn::OnPending,
            CpuState::Off => {
                *state = CpuState::Starting(request);
                cpu.wake.notify_all();
                PsciReturn::Success
            }
        }
    }

    /// Marks a core powered down. Called by the core itself.
    pub fn power_off(&self, index: u32) {
        if let Some(cpu) = self.cpus.get(index as usize) {
            *cpu.state.lock().expect("cpu state poisoned") = CpuState::Off;
        }
    }

    /// Whether a core is running, or `None` if there is no such core.
    pub fn is_on(&self, index: u32) -> Option<bool> {
        let cpu = self.cpus.get(index as usize)?;
        let state = *cpu.state.lock().expect("cpu state poisoned");
        Some(!matches!(state, CpuState::Off))
    }

    /// Blocks until this core is powered on, returning where to start.
    ///
    /// `None` means the machine is shutting down and the thread should exit.
    pub fn wait_for_power_on(&self, index: u32) -> Option<StartRequest> {
        let cpu = self.cpus.get(index as usize)?;
        let mut state = cpu.state.lock().expect("cpu state poisoned");
        loop {
            if *self.shutdown.lock().expect("shutdown flag poisoned") {
                return None;
            }
            if let CpuState::Starting(request) = *state {
                *state = CpuState::On;
                return Some(request);
            }
            // Timed wait so a shutdown that lands between the check above and
            // the wait below cannot park this thread forever.
            let (guard, _) = cpu
                .wake
                .wait_timeout(state, std::time::Duration::from_millis(200))
                .expect("cpu state poisoned");
            state = guard;
        }
    }

    /// Releases every parked core so its thread can exit.
    pub fn shutdown(&self) {
        *self.shutdown.lock().expect("shutdown flag poisoned") = true;
        for cpu in &self.cpus {
            let _guard = cpu.state.lock().expect("cpu state poisoned");
            cpu.wake.notify_all();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn request() -> StartRequest {
        StartRequest {
            entry_point: 0x4008_0000,
            context_id: 7,
        }
    }

    #[test]
    fn core_zero_starts_on_and_others_start_off() {
        let park = CpuPark::new(4);
        assert_eq!(park.is_on(0), Some(true));
        assert_eq!(park.is_on(1), Some(false));
        assert_eq!(park.is_on(4), None);
    }

    #[test]
    fn power_on_reports_the_states_linux_probes_for() {
        let park = CpuPark::new(2);
        assert_eq!(park.power_on(1, request()), PsciReturn::Success);
        // Already requested but not yet consumed by its thread.
        assert_eq!(park.power_on(1, request()), PsciReturn::OnPending);
        assert_eq!(park.power_on(0, request()), PsciReturn::AlreadyOn);
        assert_eq!(park.power_on(9, request()), PsciReturn::InvalidParams);
    }

    #[test]
    fn a_parked_core_wakes_with_its_start_request() {
        let park = Arc::new(CpuPark::new(2));
        let waiter = {
            let park = park.clone();
            std::thread::spawn(move || park.wait_for_power_on(1))
        };
        // Racing the waiter deliberately: power_on may land before or after
        // the wait begins, and both orders must deliver the request.
        assert_eq!(park.power_on(1, request()), PsciReturn::Success);
        assert_eq!(waiter.join().unwrap(), Some(request()));
        assert_eq!(park.is_on(1), Some(true));
    }

    #[test]
    fn shutdown_releases_parked_cores() {
        let park = Arc::new(CpuPark::new(2));
        let waiter = {
            let park = park.clone();
            std::thread::spawn(move || park.wait_for_power_on(1))
        };
        park.shutdown();
        assert_eq!(waiter.join().unwrap(), None, "parked core must not hang");
    }

    #[test]
    fn a_core_that_powers_off_can_be_started_again() {
        let park = CpuPark::new(2);
        park.power_on(1, request());
        park.wait_for_power_on(1);
        park.power_off(1);
        assert_eq!(park.is_on(1), Some(false));
        assert_eq!(park.power_on(1, request()), PsciReturn::Success);
    }
}
