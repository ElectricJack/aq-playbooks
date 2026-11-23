use super::*;
pub(super) struct ForEachStep {
    pub(super) collection: Expr,
    pub(super) item: String,
    pub(super) body: String,
    pub(super) next: String,
    pub(super) max_items: usize,
    pub(super) concurrency: u32,
    pub(super) on_failure: IterationFailure,
}

impl StepExecution<'_> {
    pub(super) fn for_each(&mut self, spec: ForEachStep) -> Result<()> {
        if spec.concurrency != 1 {
            return Err(Error::Refused(
                "parallel foreach unsupported; use owned async calls and joins".into(),
            ));
        }
        if self
            .run
            .loops
            .last()
            .is_some_and(|f| f.site == self.run.step)
        {
            self.advance_loop(spec.next)?;
        } else {
            self.start_loop(spec)?;
        }
        receipt(
            self.state,
            &self.run.id,
            &self.run.step,
            self.now,
            "iteration",
            json!({"path":self.run.loops.iter().map(|x|x.index).collect::<Vec<_>>()}),
        );
        Ok(())
    }

    fn advance_loop(&mut self, next: String) -> Result<()> {
        let mut frame = self.run.loops.pop().unwrap();
        frame.index += 1;
        if frame.index >= frame.items.len() {
            if let Some(previous) = frame.previous {
                self.run.variables.insert(frame.item, previous);
            } else {
                self.run.variables.remove(&frame.item);
            }
            self.run.step = next;
        } else {
            reserve_iteration(self.state, self.run, self.limits)?;
            self.run
                .variables
                .insert(frame.item.clone(), frame.items[frame.index].clone());
            self.run.step = frame.body.clone();
            self.run.loops.push(frame);
        }
        Ok(())
    }

    fn start_loop(&mut self, spec: ForEachStep) -> Result<()> {
        let ForEachStep {
            collection,
            item,
            body,
            next,
            max_items,
            concurrency: _,
            on_failure,
        } = spec;
        let values = known_expr(&collection, &self.context)?;
        let items = values
            .as_array()
            .ok_or_else(|| Error::Refused("foreach collection must be array".into()))?
            .clone();
        if items.len() > max_items {
            return Err(Error::Refused("foreach item bound exceeded".into()));
        }
        if self.run.loops.len() >= self.limits.max_iteration_depth as usize {
            return Err(Error::Refused("iteration depth exceeded".into()));
        }
        if items.is_empty() {
            self.run.step = next;
        } else {
            reserve_iteration(self.state, self.run, self.limits)?;
            let previous = self.run.variables.insert(item.clone(), items[0].clone());
            self.run.loops.push(LoopFrame {
                site: self.run.step.clone(),
                item,
                items,
                index: 0,
                body: body.clone(),
                next,
                failure: on_failure,
                previous,
            });
            self.run.step = body;
        }
        Ok(())
    }
}
