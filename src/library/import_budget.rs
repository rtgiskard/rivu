use anyhow::Result;

/// Independent retained-data domains use checked reservations. Raw input limits
/// remain separate; scoped workspace releases its charge with its owner.
#[derive(Clone, Debug, Default)]
pub(crate) struct ImportBudget(std::sync::Arc<std::sync::Mutex<usize>>);

pub(crate) const MAX_IMPORT_RETAINED_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug)]
struct RetainedLimit;

impl std::fmt::Display for RetainedLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("import exceeds retained data limit")
    }
}
impl std::error::Error for RetainedLimit {}

impl ImportBudget {
    pub(crate) fn reserve(&self, bytes: usize) -> Result<()> {
        let mut used = self.0.lock().unwrap();
        let next = used
            .checked_add(bytes)
            .filter(|next| *next <= MAX_IMPORT_RETAINED_BYTES)
            .ok_or(RetainedLimit)?;
        *used = next;
        Ok(())
    }

    pub(crate) fn release(&self, bytes: usize) {
        let mut used = self.0.lock().unwrap();
        *used = used
            .checked_sub(bytes)
            .expect("retained reservation ownership");
    }

    pub(crate) fn scoped(&self) -> ImportBudgetScope {
        ImportBudgetScope {
            budget: self.clone(),
            bytes: 0,
        }
    }

    pub(crate) fn reserve_vec<T>(&self, values: &mut Vec<T>, additional: usize) -> Result<usize> {
        let required = values.len().checked_add(additional).ok_or(RetainedLimit)?;
        if required <= values.capacity() {
            return Ok(0);
        }
        let capacity = required.max(values.capacity().saturating_mul(2)).max(4);
        let bytes = (capacity - values.capacity())
            .checked_mul(std::mem::size_of::<T>())
            .ok_or(RetainedLimit)?;
        self.reserve(bytes)?;
        values.reserve_exact(capacity - values.len());
        Ok(bytes)
    }

    #[cfg(test)]
    pub(crate) fn remaining(&self) -> usize {
        MAX_IMPORT_RETAINED_BYTES - *self.0.lock().unwrap()
    }

    pub(super) fn is_limit(error: &anyhow::Error) -> bool {
        error.downcast_ref::<RetainedLimit>().is_some()
    }
}

#[derive(Debug)]
pub(crate) struct ImportBudgetScope {
    budget: ImportBudget,
    bytes: usize,
}

impl ImportBudgetScope {
    pub(crate) fn reserve(&mut self, bytes: usize) -> Result<()> {
        self.budget.reserve(bytes)?;
        self.bytes += bytes;
        Ok(())
    }
    pub(crate) fn release(&mut self, bytes: usize) {
        self.budget.release(bytes);
        self.bytes -= bytes;
    }
    pub(crate) fn reserve_vec<T>(&mut self, values: &mut Vec<T>, additional: usize) -> Result<()> {
        self.bytes += self.budget.reserve_vec(values, additional)?;
        Ok(())
    }
}

impl Drop for ImportBudgetScope {
    fn drop(&mut self) {
        self.budget.release(self.bytes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_scopes_release_workspace_and_overflow_does_not_change_accounting() {
        let budget = ImportBudget::default();
        budget.reserve(100).unwrap();
        let remaining = budget.remaining();
        assert!(budget.reserve(usize::MAX).is_err());
        assert_eq!(budget.remaining(), remaining);
        {
            let mut workspace = budget.scoped();
            let mut values = Vec::<u64>::new();
            workspace.reserve_vec(&mut values, 7).unwrap();
            assert_eq!(budget.remaining(), remaining - values.capacity() * 8);
            workspace.reserve(300).unwrap();
            workspace.release(100);
            assert_eq!(budget.remaining(), remaining - values.capacity() * 8 - 200);
        }
        assert_eq!(budget.remaining(), remaining);
    }
}
