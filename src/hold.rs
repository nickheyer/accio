//! Which live sessions a change to the live login waits on before it is made

use accio_provider::SessionState;

// What a session has to do before the login moves under it
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wait {
    Now,
    Turn,
    Close,
}

impl Wait {
    // A session caught mid turn starts out waited on until that turn ends
    pub fn initial(state: &SessionState) -> Self {
        if state.mid_turn {
            Wait::Turn
        } else {
            Wait::Now
        }
    }

    pub fn next(self) -> Self {
        match self {
            Wait::Now => Wait::Turn,
            Wait::Turn => Wait::Close,
            Wait::Close => Wait::Now,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Wait::Now => "moves now",
            Wait::Turn => "after its turn",
            Wait::Close => "after it closes",
        }
    }
}

// One session a pending change waits on
#[derive(Clone, Debug, PartialEq)]
pub struct Hold {
    pub id: String,
    pub name: String,
    pub wait: Wait,
}

// Holds still in force, a turn hold counts whenever its session is mid turn again
pub fn pending<'a>(holds: &'a [Hold], states: &[SessionState]) -> Vec<&'a Hold> {
    holds
        .iter()
        .filter(
            |hold| match (hold.wait, states.iter().find(|state| state.id == hold.id)) {
                (Wait::Now, _) | (_, None) => false,
                (Wait::Turn, Some(state)) => state.mid_turn,
                (Wait::Close, Some(_)) => true,
            },
        )
        .collect()
}

// What has to happen before the change goes ahead, empty once nothing holds it
pub fn until(holds: &[Hold], states: &[SessionState]) -> String {
    pending(holds, states)
        .iter()
        .map(|hold| match hold.wait {
            Wait::Turn => format!("{} ends its turn", hold.name),
            _ => format!("{} closes", hold.name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(id: &str, mid_turn: bool) -> SessionState {
        SessionState {
            id: id.to_string(),
            state: String::new(),
            mid_turn,
        }
    }

    fn hold(id: &str, wait: Wait) -> Hold {
        Hold {
            id: id.to_string(),
            name: format!("s{id}"),
            wait,
        }
    }

    #[test]
    fn holds_lift_when_turns_end_or_sessions_close_and_return_with_a_new_turn() {
        let holds = [
            hold("1", Wait::Turn),
            hold("2", Wait::Close),
            hold("3", Wait::Now),
        ];
        let busy = [state("1", true), state("2", false), state("3", true)];
        assert_eq!(until(&holds, &busy), "s1 ends its turn, s2 closes");
        let rested = [state("1", false), state("2", false), state("3", true)];
        assert_eq!(until(&holds, &rested), "s2 closes");
        let closed = [state("1", false), state("3", true)];
        assert_eq!(until(&holds, &closed), "");
        let again = [state("1", true)];
        assert_eq!(until(&holds, &again), "s1 ends its turn");
        assert!(pending(&holds, &[]).is_empty());

        assert_eq!(Wait::initial(&state("1", true)), Wait::Turn);
        assert_eq!(Wait::initial(&state("1", false)), Wait::Now);
        assert_eq!(Wait::Now.next().next().next(), Wait::Now);
        assert_eq!(Wait::Turn.next().label(), "after it closes");
    }
}
