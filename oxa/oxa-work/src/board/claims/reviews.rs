use super::*;
use crate::board::{
    assignment::ManualAuthority,
    review::{Dispatch, Request, RequestRemote},
};

impl Claims {
    pub fn dispatch_review_request(
        &self,
        remote: &mut impl RequestRemote,
        request: &Request,
        authority: Option<ManualAuthority>,
    ) -> Result<Dispatch> {
        if authority.is_none() {
            return Err("owner or lead control required".into());
        }
        if !remote.current_and_admitted(request)? {
            return Ok(Dispatch::Held);
        }
        if !self.reserve_review_request(request, authority)? {
            return Ok(Dispatch::AlreadyReserved);
        }
        remote.send_once(request)?;
        Ok(Dispatch::Sent)
    }

    /// Atomically consume the request before dispatch. An uncertain result from
    /// the external reviewer leaves the reservation in place. Head changes alone
    /// never reset this reservation.
    pub fn reserve_review_request(
        &self,
        request: &Request,
        authority: Option<ManualAuthority>,
    ) -> Result<bool> {
        if authority.is_none() {
            return Err("owner or lead control required".into());
        }
        if !request.key.valid() {
            return Err("invalid review request key".into());
        }
        let _lock = self.lock(request.number)?;
        let mut row = self.load(request.number)?;
        if row.data.import_hold || row.review_requests.contains(&request.key) {
            return Ok(false);
        }
        row.review_requests.push(request.key.clone());
        let replacing = self.established(request.number)?;
        self.save(request.number, row, replacing)?;
        self.mark(request.number)?;
        Ok(true)
    }
}
