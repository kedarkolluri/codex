use super::Parser;
use crate::WORKFLOW_DESCRIPTION_MAX_BYTES;
use crate::ensure_workflow_agent_option;

impl Parser<'_> {
    pub(super) fn parse_phase_object(&mut self) -> Result<String, String> {
        self.expect_char('{', "workflow phase metadata must begin with `{`")?;
        let mut title = None;
        let mut detail = None;
        let mut model = None;
        loop {
            self.skip_trivia()?;
            if self.consume_char('}')? {
                break;
            }
            let key = self.parse_property_key()?;
            self.skip_trivia()?;
            self.expect_char(':', "workflow phase metadata properties require `:`")?;
            self.skip_trivia()?;
            let (slot, field) = match key.as_str() {
                "title" => (&mut title, "`meta.phases[].title`"),
                "detail" => (&mut detail, "`meta.phases[].detail`"),
                "model" => (&mut model, "`meta.phases[].model`"),
                _ => {
                    return Err(
                        "workflow phase metadata supports only `title`, `detail`, and `model` properties"
                            .to_string(),
                    );
                }
            };
            if slot.is_some() {
                return Err(format!("duplicate {field} property"));
            }
            *slot = Some(self.parse_string_field(field)?);
            self.expect_entry_end('}')?;
        }
        let title = title.ok_or_else(|| "`meta.phases[].title` is required".to_string())?;
        if detail
            .as_ref()
            .is_some_and(|detail: &String| detail.len() > WORKFLOW_DESCRIPTION_MAX_BYTES)
        {
            return Err(format!(
                "`meta.phases[].detail` exceeds {WORKFLOW_DESCRIPTION_MAX_BYTES} bytes"
            ));
        }
        if let Some(model) = model {
            ensure_workflow_agent_option("`meta.phases[].model`", &model)?;
        }
        Ok(title)
    }
}
