//! Typed additive facility rules. Geometry and access provide observations; mods
//! choose requirements, capacity and attributes. Evidence follows the same tree.
use crate::architecture::{PlacementId, UseSlotId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const MAX_EXPRESSION_NODES: usize = 256;
pub const MAX_EXPRESSION_DEPTH: usize = 24;
pub const MAX_VALUE: f64 = 1_000_000_000_000.;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapacityKind {
    Installed,
    Usable,
    Available,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Measurement {
    StructuralArea,
    RoofCoverage,
    MinimumClearance,
    InteriorVolume,
}
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ObservationQuery {
    Measure {
        measurement: Measurement,
    },
    Slots {
        interaction: String,
        capacity: CapacityKind,
        #[serde(default)]
        adjacent_to_tag: Option<String>,
    },
    Placements {
        tag: String,
    },
    Adjacency {
        left_tag: String,
        right_tag: String,
    },
    Environment {
        attribute: String,
    },
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Comparison {
    AtLeast,
    AtMost,
    Equal,
    Greater,
    Less,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Arithmetic {
    Add,
    Subtract,
    Multiply,
    Divide,
    Minimum,
    Maximum,
    Floor,
    Ceiling,
    Round,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Expression {
    Number {
        value: f64,
    },
    Boolean {
        value: bool,
    },
    Observe {
        query: ObservationQuery,
    },
    Compare {
        operation: Comparison,
        left: Box<Expression>,
        right: Box<Expression>,
    },
    Calculate {
        operation: Arithmetic,
        operands: Vec<Expression>,
    },
    All {
        conditions: Vec<Expression>,
    },
    Any {
        conditions: Vec<Expression>,
    },
    Not {
        condition: Box<Expression>,
    },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CapabilityScope {
    #[default]
    ArchitecturalSpace,
    Outdoor {
        anchor_interaction: String,
        range: u32,
        #[serde(default)]
        allow_portals: bool,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Requirement {
    pub key: String,
    pub description: String,
    pub condition: Expression,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CapabilityDefinition {
    pub id: String,
    pub display_name: String,
    #[serde(default)]
    pub scope: CapabilityScope,
    pub requirements: Vec<Requirement>,
    #[serde(default)]
    pub capacity: Option<Expression>,
    #[serde(default)]
    pub attributes: BTreeMap<String, Expression>,
}
#[derive(Default)]
pub struct CapabilityReferences {
    pub tags: BTreeSet<String>,
    pub interactions: BTreeSet<String>,
    pub environmental_attributes: BTreeSet<String>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueType {
    Number,
    Boolean,
}
impl ObservationQuery {
    pub fn label(&self) -> String {
        match self {
            Self::Measure { measurement } => match measurement {
                Measurement::StructuralArea => "structural floor area".into(),
                Measurement::RoofCoverage => "roof coverage ratio".into(),
                Measurement::MinimumClearance => "minimum clearance".into(),
                Measurement::InteriorVolume => "bounded interior volume".into(),
            },
            Self::Slots {
                interaction,
                capacity,
                adjacent_to_tag,
            } => format!(
                "{} {interaction} slots{}",
                match capacity {
                    CapacityKind::Installed => "installed",
                    CapacityKind::Usable => "usable",
                    CapacityKind::Available => "available",
                },
                adjacent_to_tag
                    .as_ref()
                    .map(|tag| format!(" beside {tag}"))
                    .unwrap_or_default()
            ),
            Self::Placements { tag } => format!("distinct objects tagged {tag}"),
            Self::Adjacency {
                left_tag,
                right_tag,
            } => format!("adjacent {left_tag} / {right_tag} object pairs"),
            Self::Environment { attribute } => format!("environmental attribute {attribute}"),
        }
    }
    fn validate(&self, references: &CapabilityReferences) -> Result<(), String> {
        let require = |set: &BTreeSet<String>, key: &String, category: &str| {
            if set.contains(key) {
                Ok(())
            } else {
                Err(format!("unknown {category} reference {key}"))
            }
        };
        match self {
            Self::Measure { .. } => Ok(()),
            Self::Slots {
                interaction,
                adjacent_to_tag,
                ..
            } => {
                require(&references.interactions, interaction, "interaction")?;
                if let Some(tag) = adjacent_to_tag {
                    require(&references.tags, tag, "tag")?;
                }
                Ok(())
            }
            Self::Placements { tag } => require(&references.tags, tag, "tag"),
            Self::Adjacency {
                left_tag,
                right_tag,
            } => {
                require(&references.tags, left_tag, "tag")?;
                require(&references.tags, right_tag, "tag")
            }
            Self::Environment { attribute } => require(
                &references.environmental_attributes,
                attribute,
                "environmental attribute",
            ),
        }
    }
}
impl Expression {
    pub fn observations(&self, output: &mut BTreeSet<ObservationQuery>) {
        match self {
            Self::Observe { query } => {
                output.insert(query.clone());
            }
            Self::Compare { left, right, .. } => {
                left.observations(output);
                right.observations(output);
            }
            Self::Calculate { operands, .. } => {
                for child in operands {
                    child.observations(output);
                }
            }
            Self::All { conditions } | Self::Any { conditions } => {
                for child in conditions {
                    child.observations(output);
                }
            }
            Self::Not { condition } => condition.observations(output),
            _ => {}
        }
    }
    pub fn node_count(&self) -> usize {
        1 + match self {
            Self::Compare { left, right, .. } => left.node_count() + right.node_count(),
            Self::Calculate { operands, .. } => operands.iter().map(Self::node_count).sum(),
            Self::All { conditions } | Self::Any { conditions } => {
                conditions.iter().map(Self::node_count).sum()
            }
            Self::Not { condition } => condition.node_count(),
            _ => 0,
        }
    }
}
impl Expression {
    fn validate_inner(
        &self,
        refs: &CapabilityReferences,
        depth: usize,
        nodes: &mut usize,
    ) -> Result<ValueType, String> {
        *nodes += 1;
        if depth > MAX_EXPRESSION_DEPTH || *nodes > MAX_EXPRESSION_NODES {
            return Err("capability expression exceeds depth or node budget".into());
        }
        let mut typed = |expression: &Expression, expected| {
            let actual = expression.validate_inner(refs, depth + 1, nodes)?;
            if actual != expected {
                Err(format!(
                    "expected {expected:?} operand, observed {actual:?}"
                ))
            } else {
                Ok(())
            }
        };
        match self {
            Self::Number { value } => {
                if !value.is_finite() || value.abs() > MAX_VALUE {
                    return Err("number is nonfinite or exceeds arithmetic bounds".into());
                }
                Ok(ValueType::Number)
            }
            Self::Boolean { .. } => Ok(ValueType::Boolean),
            Self::Observe { query } => {
                query.validate(refs)?;
                Ok(ValueType::Number)
            }
            Self::Compare { left, right, .. } => {
                typed(left, ValueType::Number)?;
                typed(right, ValueType::Number)?;
                Ok(ValueType::Boolean)
            }
            Self::Calculate {
                operation,
                operands,
            } => {
                let valid = match operation {
                    Arithmetic::Floor | Arithmetic::Ceiling | Arithmetic::Round => {
                        operands.len() == 1
                    }
                    Arithmetic::Subtract | Arithmetic::Divide => operands.len() == 2,
                    _ => (2..=32).contains(&operands.len()),
                };
                if !valid {
                    return Err(format!("{operation:?} has invalid operand count"));
                }
                for operand in operands {
                    typed(operand, ValueType::Number)?;
                }
                Ok(ValueType::Number)
            }
            Self::All { conditions } | Self::Any { conditions } => {
                if conditions.is_empty() || conditions.len() > 32 {
                    return Err("Boolean combination requires 1–32 conditions".into());
                }
                for condition in conditions {
                    typed(condition, ValueType::Boolean)?;
                }
                Ok(ValueType::Boolean)
            }
            Self::Not { condition } => {
                typed(condition, ValueType::Boolean)?;
                Ok(ValueType::Boolean)
            }
        }
    }
    pub fn validate(&self, refs: &CapabilityReferences) -> Result<ValueType, String> {
        self.validate_inner(refs, 0, &mut 0)
    }
}
impl CapabilityDefinition {
    pub fn validate(&self, refs: &CapabilityReferences) -> Result<(), String> {
        if self.id.is_empty()
            || self.id.len() > 128
            || self.display_name.is_empty()
            || self.display_name.len() > 256
            || self.requirements.len() > 32
            || self.attributes.len() > 32
        {
            return Err("invalid capability identity or rule count".into());
        }
        if let CapabilityScope::Outdoor {
            anchor_interaction,
            range,
            ..
        } = &self.scope
            && (*range == 0 || *range > 1024 || !refs.interactions.contains(anchor_interaction))
        {
            return Err(
                "outdoor scope requires a known anchor interaction and range 1–1024".into(),
            );
        }
        let mut keys = BTreeSet::new();
        for requirement in &self.requirements {
            if requirement.key.is_empty()
                || requirement.key.len() > 128
                || !keys.insert(&requirement.key)
                || requirement.description.is_empty()
                || requirement.description.len() > 512
            {
                return Err("requirements need unique keys and bounded descriptions".into());
            }
            if requirement.condition.validate(refs)? != ValueType::Boolean {
                return Err(format!("requirement {} must be Boolean", requirement.key));
            }
        }
        if let Some(capacity) = &self.capacity
            && capacity.validate(refs)? != ValueType::Number
        {
            return Err("capacity must be numeric".into());
        }
        for (key, expression) in &self.attributes {
            if key.is_empty() || key.len() > 128 {
                return Err("attribute key must be bounded and nonempty".into());
            }
            expression.validate(refs)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum Value {
    Unbounded,
    Number(f64),
    Boolean(bool),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "reason", rename_all = "snake_case")]
pub enum EvaluationProblem {
    Unknown(String),
    Invalid(String),
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Evidence {
    pub description: String,
    pub value: Option<Value>,
    pub problem: Option<EvaluationProblem>,
    pub placements: BTreeSet<PlacementId>,
    pub slots: BTreeSet<UseSlotId>,
    pub children: Vec<Evidence>,
}
impl Evidence {
    pub fn number(description: String, value: f64) -> Self {
        Self {
            description,
            value: Some(Value::Number(value)),
            problem: None,
            placements: BTreeSet::new(),
            slots: BTreeSet::new(),
            children: Vec::new(),
        }
    }
    pub fn unknown(description: String, reason: String) -> Self {
        let mut evidence = Self::number(description, 0.);
        evidence.value = None;
        evidence.problem = Some(EvaluationProblem::Unknown(reason));
        evidence
    }
}
pub trait CapabilityInput {
    fn observe(&self, query: &ObservationQuery) -> Evidence;
}

impl Expression {
    pub fn evaluate(&self, input: &impl CapabilityInput) -> Evidence {
        self.evaluate_inner(input, 0, &mut 0)
    }
    fn evaluate_inner(
        &self,
        input: &impl CapabilityInput,
        depth: usize,
        nodes: &mut usize,
    ) -> Evidence {
        *nodes += 1;
        if depth > MAX_EXPRESSION_DEPTH || *nodes > MAX_EXPRESSION_NODES {
            let mut result =
                Evidence::unknown("expression".into(), "expression budget exceeded".into());
            result.problem = Some(EvaluationProblem::Invalid(
                "expression budget exceeded".into(),
            ));
            return result;
        }
        let mut children = Vec::new();
        let description;
        let mut direct = None;
        match self {
            Self::Number { value } => {
                description = value.to_string();
                direct = Some(Value::Number(*value));
            }
            Self::Boolean { value } => {
                description = value.to_string();
                direct = Some(Value::Boolean(*value));
            }
            Self::Observe { query } => {
                let mut observed = input.observe(query);
                observed.description = query.label();
                if !(matches!(observed.value, Some(Value::Number(value)) if value.is_finite() && value.abs() <= MAX_VALUE)
                    || observed.value == Some(Value::Unbounded))
                    && observed.problem.is_none()
                {
                    observed.value = None;
                    observed.problem = Some(EvaluationProblem::Invalid(
                        "observation must be a finite bounded number".into(),
                    ));
                }
                return observed;
            }
            Self::Compare {
                operation,
                left,
                right,
            } => {
                description = match operation {
                    Comparison::AtLeast => "at least",
                    Comparison::AtMost => "at most",
                    Comparison::Equal => "equal to",
                    Comparison::Greater => "greater than",
                    Comparison::Less => "less than",
                }
                .into();
                children.push(left.evaluate_inner(input, depth + 1, nodes));
                children.push(right.evaluate_inner(input, depth + 1, nodes));
            }
            Self::Calculate {
                operation,
                operands,
            } => {
                description = match operation {
                    Arithmetic::Add => "sum",
                    Arithmetic::Subtract => "difference",
                    Arithmetic::Multiply => "product",
                    Arithmetic::Divide => "ratio",
                    Arithmetic::Minimum => "lesser value",
                    Arithmetic::Maximum => "greater value",
                    Arithmetic::Floor => "round down",
                    Arithmetic::Ceiling => "round up",
                    Arithmetic::Round => "round to nearest",
                }
                .into();
                children.extend(
                    operands
                        .iter()
                        .map(|child| child.evaluate_inner(input, depth + 1, nodes)),
                );
            }
            Self::All { conditions } | Self::Any { conditions } => {
                description = if matches!(self, Self::All { .. }) {
                    "all conditions"
                } else {
                    "any condition"
                }
                .into();
                children.extend(
                    conditions
                        .iter()
                        .map(|child| child.evaluate_inner(input, depth + 1, nodes)),
                );
            }
            Self::Not { condition } => {
                description = "not".into();
                children.push(condition.evaluate_inner(input, depth + 1, nodes));
            }
        }
        let mut result = Evidence {
            description,
            value: direct,
            problem: None,
            placements: children
                .iter()
                .flat_map(|child| child.placements.iter().copied())
                .collect(),
            slots: children
                .iter()
                .flat_map(|child| child.slots.iter().cloned())
                .collect(),
            children,
        };
        let invalid = |reason: &str| EvaluationProblem::Invalid(reason.into());
        if let Some(problem) = result
            .children
            .iter()
            .filter_map(|child| child.problem.as_ref())
            .find(|problem| matches!(problem, EvaluationProblem::Invalid(_)))
        {
            result.problem = Some(problem.clone());
            return result;
        }
        let numbers: Option<Vec<f64>> = result
            .children
            .iter()
            .map(|child| match child.value {
                Some(Value::Number(value)) => Some(value),
                Some(Value::Unbounded) => Some(f64::INFINITY),
                _ => None,
            })
            .collect();
        let booleans: Option<Vec<bool>> = result
            .children
            .iter()
            .map(|child| match child.value {
                Some(Value::Boolean(value)) => Some(value),
                _ => None,
            })
            .collect();
        match self {
            Self::Compare { operation, .. } => {
                if let Some(values) = numbers
                    && values.len() == 2
                {
                    result.value = Some(Value::Boolean(match operation {
                        Comparison::AtLeast => values[0] >= values[1],
                        Comparison::AtMost => values[0] <= values[1],
                        Comparison::Equal => values[0] == values[1],
                        Comparison::Greater => values[0] > values[1],
                        Comparison::Less => values[0] < values[1],
                    }));
                }
            }
            Self::Calculate { operation, .. } => {
                if let Some(values) = numbers {
                    let value = match (operation, values.as_slice()) {
                        (Arithmetic::Add, [_, _, ..]) => Some(values.iter().sum()),
                        (Arithmetic::Multiply, [_, _, ..]) => Some(values.iter().product()),
                        (Arithmetic::Minimum, [_, _, ..]) => {
                            values.iter().copied().reduce(f64::min)
                        }
                        (Arithmetic::Maximum, [_, _, ..]) => {
                            values.iter().copied().reduce(f64::max)
                        }
                        (Arithmetic::Subtract, [a, b]) => Some(a - b),
                        (Arithmetic::Divide, [a, b]) if *b != 0. => Some(a / b),
                        (Arithmetic::Divide, [_, _]) => {
                            result.problem = Some(invalid("division by zero"));
                            None
                        }
                        (Arithmetic::Floor, [value]) => Some(value.floor()),
                        (Arithmetic::Ceiling, [value]) => Some(value.ceil()),
                        (Arithmetic::Round, [value]) => Some(value.round()),
                        _ => {
                            result.problem = Some(invalid("invalid arithmetic operands"));
                            None
                        }
                    };
                    result.value = value.map(Value::Number);
                }
            }
            Self::All { .. } | Self::Any { .. } => {
                let all = matches!(self, Self::All { .. });
                // A known decisive Boolean result does not become unknown merely
                // because an irrelevant branch still awaits geometry.
                if result
                    .children
                    .iter()
                    .any(|child| child.value == Some(Value::Boolean(!all)))
                {
                    result.value = Some(Value::Boolean(!all));
                } else if let Some(values) = booleans
                    && !values.is_empty()
                {
                    result.value = Some(Value::Boolean(all));
                }
            }
            Self::Not { .. } => {
                if let Some(values) = booleans
                    && values.len() == 1
                {
                    result.value = Some(Value::Boolean(!values[0]));
                }
            }
            _ => {}
        }
        if let Some(Value::Number(value)) = result.value
            && (!value.is_finite() || value.abs() > MAX_VALUE)
        {
            result.value = None;
            result.problem = Some(invalid("arithmetic result exceeds finite bounds"));
        }
        if result.value.is_none() && result.problem.is_none() {
            result.problem = result
                .children
                .iter()
                .find_map(|child| child.problem.clone())
                .or_else(|| Some(invalid("operand type mismatch")));
        }
        result
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Pending,
    Invalid,
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityResult {
    pub id: String,
    pub display_name: String,
    pub status: CapabilityStatus,
    pub requirements: BTreeMap<String, Evidence>,
    pub capacity: Option<Evidence>,
    pub attributes: BTreeMap<String, Evidence>,
}
impl CapabilityDefinition {
    pub fn evaluate(&self, input: &impl CapabilityInput) -> CapabilityResult {
        let requirements: BTreeMap<_, _> = self
            .requirements
            .iter()
            .map(|requirement| {
                let mut evidence = requirement.condition.evaluate(input);
                evidence.description = requirement.description.clone();
                (requirement.key.clone(), evidence)
            })
            .collect();
        let mut capacity = self
            .capacity
            .as_ref()
            .map(|expression| expression.evaluate(input));
        if let Some(evidence) = &mut capacity
            && let Some(Value::Number(value)) = evidence.value
            && (value < 0. || value.fract() != 0.)
        {
            evidence.value = None;
            evidence.problem = Some(EvaluationProblem::Invalid(
                "capacity must be a nonnegative whole number".into(),
            ));
        }
        let attributes: BTreeMap<_, _> = self
            .attributes
            .iter()
            .map(|(key, expression)| (key.clone(), expression.evaluate(input)))
            .collect();
        let results: Vec<_> = requirements
            .values()
            .chain(capacity.iter())
            .chain(attributes.values())
            .collect();
        let status = if results
            .iter()
            .any(|evidence| matches!(evidence.problem, Some(EvaluationProblem::Invalid(_))))
        {
            CapabilityStatus::Invalid
        } else if requirements
            .values()
            .any(|evidence| evidence.value == Some(Value::Boolean(false)))
        {
            CapabilityStatus::Unsupported
        } else if results.iter().any(|evidence| evidence.problem.is_some()) {
            CapabilityStatus::Pending
        } else {
            CapabilityStatus::Supported
        };
        CapabilityResult {
            id: self.id.clone(),
            display_name: self.display_name.clone(),
            status,
            requirements,
            capacity,
            attributes,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Input {
        area: Option<f64>,
        usable: f64,
    }
    impl CapabilityInput for Input {
        fn observe(&self, query: &ObservationQuery) -> Evidence {
            match query {
                ObservationQuery::Measure {
                    measurement: Measurement::StructuralArea,
                } => match self.area {
                    Some(area) => Evidence::number(query.label(), area),
                    None => Evidence::unknown(query.label(), "unavailable geometry".into()),
                },
                ObservationQuery::Slots { .. } => {
                    let mut evidence = Evidence::number(query.label(), self.usable);
                    evidence.slots.insert(UseSlotId {
                        placement: PlacementId(7),
                        key: "bed".into(),
                    });
                    evidence
                }
                _ => Evidence::unknown(query.label(), "unsupported observation".into()),
            }
        }
    }
    fn comfort(divisor: f64) -> Expression {
        serde_json::from_value(serde_json::json!({"kind":"calculate","operation":"minimum","operands":[
            {"kind":"observe","query":{"kind":"slots","interaction":"sleep","capacity":"usable"}},
            {"kind":"calculate","operation":"floor","operands":[{"kind":"calculate","operation":"divide","operands":[
                {"kind":"observe","query":{"kind":"measure","measurement":"structural_area"}},
                {"kind":"number","value":divisor}
            ]}]}
        ]})).unwrap()
    }
    #[test]
    fn area_capacity_keeps_evidence_and_expansion_changes_the_result() {
        let references = CapabilityReferences {
            interactions: BTreeSet::from(["sleep".into()]),
            ..Default::default()
        };
        let expression = comfort(4.);
        assert_eq!(expression.validate(&references), Ok(ValueType::Number));
        let small = expression.evaluate(&Input {
            area: Some(7.),
            usable: 3.,
        });
        let larger = expression.evaluate(&Input {
            area: Some(16.),
            usable: 3.,
        });
        assert_eq!(small.value, Some(Value::Number(1.)));
        assert_eq!(larger.value, Some(Value::Number(3.)));
        assert_eq!(small.slots, larger.slots);
        assert_eq!(
            small.children[1].children[0].children[0].value,
            Some(Value::Number(7.))
        );
    }
    #[test]
    fn zero_division_unknown_geometry_and_wrong_types_never_fabricate_capacity() {
        let result = comfort(0.).evaluate(&Input {
            area: Some(16.),
            usable: 3.,
        });
        assert_eq!(result.value, None);
        assert!(matches!(
            result.problem,
            Some(EvaluationProblem::Invalid(_))
        ));
        let result = comfort(4.).evaluate(&Input {
            area: None,
            usable: 3.,
        });
        assert_eq!(result.value, None);
        assert!(matches!(
            result.problem,
            Some(EvaluationProblem::Unknown(_))
        ));
        let invalid = Expression::Calculate {
            operation: Arithmetic::Floor,
            operands: vec![Expression::Boolean { value: true }],
        };
        assert!(invalid.validate(&CapabilityReferences::default()).is_err());
        assert!(
            comfort(4.)
                .validate(&CapabilityReferences::default())
                .is_err()
        );
    }
    #[test]
    fn a_decisive_boolean_branch_retains_unknown_evidence_without_guessing() {
        let unknown = Expression::Compare {
            operation: Comparison::AtLeast,
            left: Box::new(Expression::Observe {
                query: ObservationQuery::Measure {
                    measurement: Measurement::StructuralArea,
                },
            }),
            right: Box::new(Expression::Number { value: 1. }),
        };
        let expression = Expression::All {
            conditions: vec![Expression::Boolean { value: false }, unknown],
        };
        let result = expression.evaluate(&Input {
            area: None,
            usable: 0.,
        });
        assert_eq!(result.value, Some(Value::Boolean(false)));
        assert!(result.problem.is_none());
        assert!(matches!(
            result.children[1].problem,
            Some(EvaluationProblem::Unknown(_))
        ));
    }
    #[test]
    fn runaway_expression_and_arithmetic_overflow_fail_with_bounded_evidence() {
        let mut expression = Expression::Boolean { value: true };
        for _ in 0..MAX_EXPRESSION_DEPTH + 2 {
            expression = Expression::Not {
                condition: Box::new(expression),
            };
        }
        assert!(
            expression
                .validate(&CapabilityReferences::default())
                .is_err()
        );
        assert!(matches!(
            expression
                .evaluate(&Input {
                    area: None,
                    usable: 0.
                })
                .problem,
            Some(EvaluationProblem::Invalid(_))
        ));
        let overflow = Expression::Calculate {
            operation: Arithmetic::Multiply,
            operands: vec![
                Expression::Number { value: MAX_VALUE },
                Expression::Number { value: MAX_VALUE },
            ],
        };
        assert!(matches!(
            overflow
                .evaluate(&Input {
                    area: None,
                    usable: 0.
                })
                .problem,
            Some(EvaluationProblem::Invalid(_))
        ));
    }
}
