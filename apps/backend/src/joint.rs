//! The joint portfolio slate: one slate that plans across all the members of a portfolio of
//! coupled action points, drawing on both capabilities' catalogs.
//!
//! The neutral `GroupAgent` grounds each member through its own capability's bounded agent
//! (the member router below) and asks one joint prompt; the neutral `GroupSlateAgent` runs
//! the shared slate loop. The capability-specific parts live here: the joint prompt, the
//! per-block parser dispatching each action to its capability's parser, and the
//! [`PortfolioScorer`], which ranks first by how many members the monitor's own rules still
//! flag in a projection. A projection commits nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::sync::Arc;

use async_trait::async_trait;
use ekg_cognition::{
    BoundedAgent, GroupAgent, GroupAgentProposer, GroupSlateAgent, MemberRouter, RunGate,
    RunRecorder,
};
use ekg_domain::{
    CognitionProvider, EngineError, EntityUri, Grounding, GroupScorer, Plan, Projection,
    ProposedAction, Result, Score, ScoredCandidate, Scorer, Simulator, SubjectGroup, WorldModel,
    entity_model,
};
use ekg_dynamic::RiskRule;
use ekg_semantic::{ProcessContentionScorer, SupplyRiskScorer, display};

use crate::propose::{
    self, AgentSeams, PROCESS_VERBS, SUPPLY_RISK_VERBS, action_blocks, cause_chain_summary,
    grounded_entities, grounded_work_orders, parse_process_block, parse_supply_block,
    process_catalog, process_catalog_targets, supply_catalog,
};

/// Width the flagged-member count is zero-padded to in the joint `rank_key`, so lexical
/// order is numeric order.
const MEMBER_FIELD_WIDTH: usize = 5;

/// Route a member to the bounded agent of the capability that owns it: a manufacturing
/// subject (`mrp.*`) to the process agent, every other subject to the supply-risk agent,
/// the same routing as the point slates' router.
#[must_use]
pub fn member_router(supply_risk: Arc<BoundedAgent>, process: Arc<BoundedAgent>) -> MemberRouter {
    Arc::new(move |member: &EntityUri| {
        if is_process_member(member) {
            Arc::clone(&process)
        } else {
            Arc::clone(&supply_risk)
        }
    })
}

/// The joint prompt: the members and why they are coupled, the merged evidence, the
/// supply-risk cause chain, the process work orders and the orders and capacity records a
/// process remediation may act on, and both catalogs, then the revise feedback. It asks for
/// the smallest plan resolving every member, in the same block format as a point prompt.
#[must_use]
pub fn group_prompt(
    group: &SubjectGroup,
    grounding: &Grounding,
    feedback: &[ScoredCandidate],
) -> String {
    let mut members = String::new();
    for member in &group.members {
        let _ = writeln!(members, "- {member}");
    }
    let mut links = String::new();
    for link in &group.links {
        let _ = writeln!(links, "- {link}");
    }
    let mut evidence = String::new();
    for edge in &grounding.edges {
        let _ = writeln!(
            evidence,
            "- {} {} {}",
            edge.subject, edge.predicate, edge.object
        );
    }
    if grounding.has_omissions() {
        let _ = writeln!(
            evidence,
            "- ({} fact(s) hidden by read policy)",
            grounding.omissions.len()
        );
    }
    let mut work_orders = String::new();
    for wo in grounded_work_orders(grounding) {
        let _ = writeln!(work_orders, "  {wo}");
    }
    let mut revise = String::new();
    if !feedback.is_empty() {
        let _ = writeln!(
            revise,
            "\nAlready tried, with the simulated outcome (propose a DIFFERENT plan):"
        );
        for tried in feedback {
            let plan = tried
                .plan
                .actions
                .iter()
                .map(|a| {
                    let params = a
                        .params
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("{} {} [{params}]", a.action_type.0, a.target.0)
                })
                .collect::<Vec<_>>()
                .join(" + ");
            let outcome = tried
                .score
                .metrics
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join(", ");
            let _ = writeln!(revise, "  - {plan} → {outcome}");
        }
    }
    format!(
        "You are a planner for a portfolio of coupled action points. They must be resolved \
         together: a change to one changes the others.\n\
         Members:\n{members}\
         Why they are coupled:\n{links}\
         Evidence (typed, provenance-bearing):\n{evidence}\n\
         Supply-risk cause chain (the short components' open inbound lines):\n{chain}\n\
         Work orders you may act on:\n{work_orders}{targets}\n\
         Governed supply-risk actions you may propose:\n{supply}\
         Governed process actions you may propose:\n{process}{revise}\n\
         Target only entities named above. Prefer the smallest plan that resolves EVERY member \
         at once; it may combine supply-risk and process actions. Explain briefly, then end \
         your reply with an ACTION and a TARGET line and one PARAM line per param, per action \
         (repeat the block for each):\n\
         \u{20}\u{20}ACTION: <verb>\n  TARGET: <entity IRI>\n  PARAM: <key>=<value>\n\
         Do not invent verbs, targets or parameters, and take no action yourself.",
        chain = cause_chain_summary(grounding),
        targets = process_catalog_targets(grounding),
        supply = supply_catalog(),
        process = process_catalog(),
    )
}

/// Parse a joint reply into one plan: each `ACTION:` block is dispatched by its verb to its
/// capability's own block parser, which validates it against the merged grounding and the
/// kinetic catalog as a point parser does. A verb in neither catalog, or any refused
/// block, refuses the whole plan; a reply with no `ACTION:` line is an abstention.
///
/// # Errors
/// [`EngineError::InvalidProposal`] naming the first refused block.
pub fn group_parse(reply: &str, grounding: &Grounding) -> Result<Vec<ProposedAction>> {
    let blocks = action_blocks(reply);
    if blocks.is_empty() {
        return Err(EngineError::InvalidProposal(
            "no ACTION: line in the joint reply".to_owned(),
        ));
    }
    let grounded: BTreeSet<String> = grounded_entities(&grounding.entity, grounding);
    blocks
        .iter()
        .map(|block| {
            let verb = block
                .iter()
                .find_map(|line| line.strip_prefix("ACTION:"))
                .map(str::trim)
                .unwrap_or_default();
            if SUPPLY_RISK_VERBS.iter().any(|v| v.name() == verb) {
                parse_supply_block(block, &grounded)
            } else if PROCESS_VERBS.iter().any(|v| v.name() == verb) {
                parse_process_block(block, grounding)
            } else {
                Err(EngineError::InvalidProposal(format!(
                    "proposed verb {verb:?} is in neither the supply-risk nor the process catalog"
                )))
            }
        })
        .collect()
}

/// The joint residual risk: the sum of each capability's `risk_left` count that parses.
/// A count a capability does not serve is left out, not read as zero; with none served,
/// `None`.
fn residual_risk(scores: &[&BTreeMap<String, String>]) -> Option<u64> {
    scores
        .iter()
        .filter_map(|metrics| metrics.get(display::RISK_LEFT_KEY)?.parse::<u64>().ok())
        .reduce(u64::saturating_add)
}

/// The joint scorer: the neutral primary rank is how many of the group's members the
/// monitor's own detection rules still flag in the projection (fewer first), then the
/// supply-risk scorer's key, then the process scorer's key, and last, through
/// [`GroupScorer::score_group_plan`], the plan's cost, so cost never outweighs a capability's
/// judgment. Admission (strictly better than the baseline) is the slate loop's own rule.
/// Scoring reads only the projection.
///
/// The joint score carries the plan's residual risk under the same opaque key a point's score
/// uses (`risk_left`): the sum of the capabilities' residual risk counts that parse, so the
/// display tiers its severity and draws its risk-left bars as for a point.
pub struct PortfolioScorer {
    rules: Arc<[RiskRule]>,
    supply: SupplyRiskScorer,
    process: ProcessContentionScorer,
}

impl PortfolioScorer {
    /// A scorer over the monitor's rule set.
    #[must_use]
    pub fn new(rules: Arc<[RiskRule]>) -> Self {
        Self {
            rules,
            supply: SupplyRiskScorer::new(),
            process: ProcessContentionScorer::new(),
        }
    }
}

#[async_trait]
impl GroupScorer for PortfolioScorer {
    async fn score_group(
        &self,
        projection: &dyn Projection,
        group: &SubjectGroup,
    ) -> Result<Score> {
        let verdicts = ekg_dynamic::projected_verdicts(projection, &self.rules).await?;
        let flagged: BTreeSet<&EntityUri> = verdicts
            .iter()
            .map(|verdict| &verdict.subject)
            .filter(|subject| group.members.contains(subject))
            .collect();
        // Which constraint held, for the next planner: not a part of the rank.
        let held: Vec<String> = verdicts
            .iter()
            .filter(|verdict| flagged.contains(&verdict.subject))
            .map(|verdict| format!("{}: {}", verdict.subject.0, verdict.reason))
            .collect();
        let supply = self.supply.score(projection).await?;
        let process = self.process.score(projection).await?;

        let flagged = flagged.len();
        let mut metrics = BTreeMap::from([
            ("members_at_risk".to_owned(), flagged.to_string()),
            ("members".to_owned(), group.members.len().to_string()),
            (
                "signal_cleared".to_owned(),
                if flagged == 0 { "true" } else { "false" }.to_owned(),
            ),
        ]);
        if !held.is_empty() {
            metrics.insert("still_flagged".to_owned(), held.join("; "));
        }
        if let Some(risk_left) = residual_risk(&[&supply.metrics, &process.metrics]) {
            metrics.insert(display::RISK_LEFT_KEY.to_owned(), risk_left.to_string());
        }
        for (key, value) in supply.metrics {
            if key == display::LATE_ORDERS_KEY || key == display::LATE_UNITS_KEY {
                metrics.insert(key, value);
            } else {
                metrics.insert(format!("supply_risk.{key}"), value);
            }
        }
        for (key, value) in process.metrics {
            metrics.insert(format!("process.{key}"), value);
        }
        Ok(Score {
            metrics,
            rank_key: format!(
                "{flagged:0width$}|{}|{}",
                supply.rank_key,
                process.rank_key,
                width = MEMBER_FIELD_WIDTH
            ),
        })
    }

    /// [`GroupScorer::score_group`] with the plan's cost appended as the last field:
    /// `members|supply|process|cost`, where the supply key carries no cost of its own.
    async fn score_group_plan(
        &self,
        plan: &Plan,
        projection: &dyn Projection,
        group: &SubjectGroup,
    ) -> Result<Score> {
        let mut score = self.score_group(projection, group).await?;
        let cost = display::plan_cost_key(projection, plan).await?;
        score.rank_key = format!("{}|{cost}", score.rank_key);
        Ok(score)
    }

    /// "{at risk} of {members} points still at risk", from the joint keys, then the
    /// capability's wording of the rest ([`display::describe_score`]).
    fn describe(&self, score: &Score) -> Option<String> {
        let count = |key: &str| score.metrics.get(key)?.parse::<usize>().ok();
        let members = match (count("members_at_risk"), count("members")) {
            (Some(at_risk), Some(members)) => {
                let noun = if members == 1 { "point" } else { "points" };
                Some(format!("{at_risk} of {members} {noun} still at risk"))
            }
            _ => None,
        };
        let clauses: Vec<String> = members
            .into_iter()
            .chain(display::describe_score(&score.metrics))
            .collect();
        (!clauses.is_empty()).then(|| clauses.join(" · "))
    }
}

/// Build the joint portfolio slate agent: the neutral group agent over both capabilities'
/// bounded agents (each member grounded under its own read context), the injected simulator,
/// and the [`PortfolioScorer`] over the monitor's rules, bounded to `max_actions` per plan.
#[must_use]
pub fn portfolio_slate_agent(
    world: &Arc<dyn WorldModel>,
    cognition: &Arc<dyn CognitionProvider>,
    simulator: Arc<dyn Simulator>,
    rules: Arc<[RiskRule]>,
    max_actions: usize,
) -> GroupSlateAgent {
    let supply = Arc::new(propose::supply_risk_agent(
        Arc::clone(world),
        Arc::clone(cognition),
        max_actions,
    ));
    let process = Arc::new(propose::process_agent(
        Arc::clone(world),
        Arc::clone(cognition),
        max_actions,
    ));
    let proposer = GroupAgent::new(
        member_router(supply, process),
        Arc::clone(cognition),
        Arc::new(group_prompt),
        Arc::new(group_parse),
        max_actions,
    );
    GroupSlateAgent::new(
        Arc::new(proposer),
        simulator,
        Arc::new(PortfolioScorer::new(rules)),
    )
}

/// The agent's joint slate agent for one run, over `seams`, each candidate gated and recorded
/// by `gate` and `recorder`: the same slate loop, simulator and [`PortfolioScorer`] as
/// [`portfolio_slate_agent`], with the tool-using agent drafting the candidates under the
/// portfolio profile instead of the joint prompt.
#[must_use]
pub fn agent_portfolio_slate_agent(
    seams: &AgentSeams,
    gate: &Arc<dyn RunGate>,
    recorder: &Arc<dyn RunRecorder>,
) -> GroupSlateAgent {
    let proposer = GroupAgentProposer::new(
        Arc::clone(&seams.world),
        Arc::clone(&seams.portfolio),
        Arc::clone(&seams.tokens),
        Arc::clone(&seams.reasoner),
        Arc::clone(&seams.ledger),
        Arc::clone(&seams.pins),
        {
            let text_bound = seams.text_max_chars;
            Arc::new(move |group: &_, grounding: &_, findings: &_, tried: &_| {
                propose::portfolio_task(group, grounding, findings, tried, text_bound)
            })
        },
        seams.joint_limits,
        propose::member_analysts(
            Arc::clone(&seams.findings),
            Arc::clone(&seams.supply_risk_analyst),
            Arc::clone(&seams.process_analyst),
            seams.analyst_limits,
        ),
    )
    .with_strategies(seams.planner_strategies.clone())
    .with_planner_reads(seams.planner_reads)
    .with_gate(Arc::clone(gate))
    .with_recorder(Arc::clone(recorder));
    GroupSlateAgent::new(
        Arc::new(proposer),
        Arc::clone(&seams.simulator),
        Arc::new(PortfolioScorer::new(Arc::clone(&seams.rules))),
    )
    .with_budget(seams.budget)
}

/// Whether `member` is a manufacturing subject (routed to the process agent).
#[must_use]
pub fn is_process_member(member: &EntityUri) -> bool {
    entity_model(&member.0).is_some_and(|m| m.starts_with("mrp."))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use ekg_domain::{EvidenceEdge, Revision, SourceRef};
    use ekg_dynamic::DriftVerdict;

    use super::*;

    const SO: &str = "https://ekg.example/id/sale.order/1";
    const LINE: &str = "https://ekg.example/id/purchase.order.line/4";
    const PO: &str = "https://ekg.example/id/purchase.order/1";
    const MO: &str = "https://ekg.example/id/mrp.production/7";
    const WO: &str = "https://ekg.example/id/mrp.workorder/3";

    fn edge(subject: &str, predicate: &str, object: &str) -> EvidenceEdge {
        EvidenceEdge {
            subject: EntityUri(subject.to_owned()),
            predicate: predicate.to_owned(),
            object: object.to_owned(),
            source: SourceRef {
                system: "odoo".to_owned(),
                model: entity_model(subject).unwrap_or("x").to_owned(),
                id: 1,
            },
            revision: Revision(1),
        }
    }

    fn group() -> SubjectGroup {
        SubjectGroup {
            key: "portfolio/0123456789ab".to_owned(),
            members: vec![EntityUri(MO.to_owned()), EntityUri(SO.to_owned())],
            links: vec![format!("linked: {MO} fulfils {SO}")],
            worded: false,
        }
    }

    /// The members' merged grounding: the order and its cause line, the manufacturing order
    /// and its work order.
    fn merged() -> Grounding {
        Grounding {
            entity: EntityUri(MO.to_owned()),
            edges: vec![
                edge(MO, "ekg:has_work_order", WO),
                edge(MO, "ekg:product_qty", "40"),
                edge(SO, "ekg:commitment_date", "2027-10-26T00:00:00"),
                edge(
                    LINE,
                    "ekg:po_line_product",
                    "https://ekg.example/id/product.product/9",
                ),
                edge(LINE, "ekg:product_qty", "50"),
                edge(LINE, "ekg:date_planned", "2027-10-27T08:00:00"),
                edge(PO, "ekg:has_po_line", LINE),
            ],
            omissions: Vec::new(),
        }
    }

    #[test]
    fn the_joint_prompt_names_every_member_link_and_both_catalogs() {
        let prompt = group_prompt(&group(), &merged(), &[]);
        for needle in [
            MO,
            SO,
            "fulfils",
            "expedite_po",
            "re_promise_date",
            "reschedule_work_order",
            "split_production",
            "raise_work_center_capacity",
            WO,
            LINE,
        ] {
            assert!(prompt.contains(needle), "{needle} in {prompt}");
        }
    }

    #[test]
    fn a_mixed_reply_parses_into_one_plan() {
        let reply = format!(
            "Split, pull the controllers in, and start earlier.\n\
             ACTION: split_production\nTARGET: {MO}\nPARAM: split_quantity=10\nPARAM: new_date=2027-10-24 08:00:00\n\
             ACTION: expedite_po\nTARGET: {LINE}\nPARAM: new_date=2027-10-24 08:00:00\n\
             ACTION: reschedule_work_order\nTARGET: {WO}\nPARAM: new_date=2027-10-21 08:00:00"
        );
        let plan = group_parse(&reply, &merged()).expect("one plan");
        let verbs: Vec<&str> = plan.iter().map(|a| a.action_type.0.as_str()).collect();
        assert_eq!(
            verbs,
            ["split_production", "expedite_po", "reschedule_work_order"]
        );
    }

    #[test]
    fn an_unknown_verb_refuses_the_joint_plan() {
        let reply = format!(
            "ACTION: expedite_po\nTARGET: {LINE}\nPARAM: new_date=2027-10-24 08:00:00\n\
             ACTION: add_saturday_shift\nTARGET: {WO}\nPARAM: new_date=2027-10-23 08:00:00"
        );
        let err = group_parse(&reply, &merged()).expect_err("refused");
        assert!(err.to_string().contains("neither"), "{err}");
    }

    /// A projection answering the fixture rule, the supply-risk rule, the contention rule and
    /// a purchase target's cost, and nothing else.
    #[derive(Default)]
    struct Canned {
        flagged: Vec<&'static str>,
        risk: Vec<BTreeMap<String, String>>,
        contention: Vec<BTreeMap<String, String>>,
        cost: Option<&'static str>,
    }

    #[async_trait]
    impl Projection for Canned {
        async fn select(&self, sparql: &str) -> Result<Vec<BTreeMap<String, String>>> {
            if sparql == "FIXTURE RULE" {
                return Ok(self
                    .flagged
                    .iter()
                    .map(|e| BTreeMap::from([("entity".to_owned(), (*e).to_owned())]))
                    .collect());
            }
            if sparql == ekg_semantic::ontology::RISK_RULE_SELECT_SPARQL {
                return Ok(self.risk.clone());
            }
            if sparql == ekg_semantic::ontology::PROCESS_CONTENTION_SELECT_SPARQL {
                return Ok(self.contention.clone());
            }
            if sparql.contains("(SUM(?price) AS ?cost)") {
                return Ok(self
                    .cost
                    .iter()
                    .map(|c| BTreeMap::from([("cost".to_owned(), (*c).to_owned())]))
                    .collect());
            }
            Ok(Vec::new())
        }
    }

    fn rules() -> Arc<[RiskRule]> {
        Arc::from(vec![RiskRule::new("FIXTURE RULE", |row| {
            Some(DriftVerdict {
                subject: EntityUri(row.get("entity")?.clone()),
                reason: "fixture".to_owned(),
            })
        })])
    }

    fn short(qty: &str) -> BTreeMap<String, String> {
        BTreeMap::from([
            ("so".to_owned(), SO.to_owned()),
            (
                "comp".to_owned(),
                "https://ekg.example/id/product.product/9".to_owned(),
            ),
            ("shortfall".to_owned(), qty.to_owned()),
            ("ss".to_owned(), "0".to_owned()),
        ])
    }

    #[tokio::test]
    async fn fewer_flagged_members_rank_first_regardless_of_the_capability_keys() {
        let scorer = PortfolioScorer::new(rules());
        // No member flagged, but the supply-risk key is poor (three short rows).
        let clears = scorer
            .score_group(
                &Canned {
                    flagged: vec!["https://ekg.example/id/sale.order/99"],
                    risk: vec![short("13"), short("13"), short("13")],
                    ..Canned::default()
                },
                &group(),
            )
            .await
            .expect("score");
        // One member still flagged, with a perfect supply-risk key.
        let leaves_one = scorer
            .score_group(
                &Canned {
                    flagged: vec![MO],
                    ..Canned::default()
                },
                &group(),
            )
            .await
            .expect("score");
        assert!(
            clears.rank_key < leaves_one.rank_key,
            "{} vs {}",
            clears.rank_key,
            leaves_one.rank_key
        );
        assert_eq!(
            clears.metrics.get("members_at_risk").map(String::as_str),
            Some("0"),
            "an outsider the rule flags is not a member"
        );
        assert_eq!(
            clears.metrics.get("signal_cleared").map(String::as_str),
            Some("true")
        );
        assert_eq!(
            leaves_one
                .metrics
                .get("members_at_risk")
                .map(String::as_str),
            Some("1")
        );
    }

    #[tokio::test]
    async fn the_joint_metrics_carry_the_members_the_display_keys_and_the_prefixed_rest() {
        let score = PortfolioScorer::new(rules())
            .score_group(
                &Canned {
                    flagged: vec![SO],
                    risk: vec![short("13")],
                    ..Canned::default()
                },
                &group(),
            )
            .await
            .expect("score");
        for key in [
            "members_at_risk",
            "members",
            "late_orders",
            "late_units",
            "supply_risk.risk_left",
            "supply_risk.signal_cleared",
            "process.risk_left",
            "process.residual_overlap",
        ] {
            assert!(
                score.metrics.contains_key(key),
                "{key} in {:?}",
                score.metrics
            );
        }
        assert_eq!(
            score.metrics.get("risk_left").map(String::as_str),
            Some("1"),
            "the joint residual risk: 1 at-risk row, nothing over capacity"
        );
        assert_eq!(score.metrics.get("members").map(String::as_str), Some("2"));
    }

    /// A contention row over its capacity of 1.
    fn over_capacity() -> BTreeMap<String, String> {
        BTreeMap::from([
            ("wo".to_owned(), WO.to_owned()),
            ("capacity".to_owned(), "1".to_owned()),
            ("overlapPeers".to_owned(), "2".to_owned()),
        ])
    }

    #[tokio::test]
    async fn the_joint_score_carries_its_residual_risk_and_severity() {
        let scorer = PortfolioScorer::new(rules());
        let baseline = scorer
            .score_group(
                &Canned {
                    flagged: vec![SO, MO],
                    risk: vec![short("13"), short("10")],
                    contention: vec![over_capacity(), over_capacity(), over_capacity()],
                    ..Canned::default()
                },
                &group(),
            )
            .await
            .expect("score");
        assert_eq!(
            baseline.metrics.get("risk_left").map(String::as_str),
            Some("5")
        );
        assert_eq!(
            crate::graphql::model::with_derived_display(&baseline.metrics)
                .get("severity")
                .map(String::as_str),
            Some(display::SEVERITY_CRITICAL)
        );
        let option = scorer
            .score_group(
                &Canned {
                    flagged: vec![SO],
                    risk: vec![short("13"), short("10")],
                    ..Canned::default()
                },
                &group(),
            )
            .await
            .expect("score");
        assert_eq!(
            option.metrics.get("risk_left").map(String::as_str),
            Some("2")
        );
    }

    #[test]
    fn a_count_a_capability_does_not_serve_is_left_out_of_the_residual_risk() {
        let count = |v: &str| BTreeMap::from([("risk_left".to_owned(), v.to_owned())]);
        assert_eq!(residual_risk(&[&count("2"), &count("3")]), Some(5));
        assert_eq!(residual_risk(&[&count("2"), &BTreeMap::new()]), Some(2));
        assert_eq!(residual_risk(&[&count("n/a"), &count("0")]), Some(0));
        assert_eq!(residual_risk(&[&BTreeMap::new(), &count("n/a")]), None);
    }

    fn plan(targets: &[&str]) -> Plan {
        Plan {
            actions: targets
                .iter()
                .map(|t| ProposedAction {
                    action_type: ekg_domain::ActionType("expedite_po".to_owned()),
                    target: EntityUri((*t).to_owned()),
                    params: BTreeMap::new(),
                })
                .collect(),
        }
    }

    #[tokio::test]
    async fn cost_only_breaks_a_full_joint_tie() {
        let scorer = PortfolioScorer::new(rules());
        let world = |risk: Vec<BTreeMap<String, String>>| Canned {
            flagged: vec![SO],
            risk,
            cost: Some("320.0"),
            ..Canned::default()
        };
        let paid = scorer
            .score_group_plan(&plan(&[LINE]), &world(vec![short("13")]), &group())
            .await
            .expect("score");
        let free = scorer
            .score_group_plan(&plan(&[WO]), &world(vec![short("13")]), &group())
            .await
            .expect("score");
        assert!(
            paid.rank_key.ends_with("|0000000000320.00"),
            "{}",
            paid.rank_key
        );
        assert_eq!(
            paid.rank_key.split('|').count(),
            4,
            "members|supply|process|cost"
        );
        assert!(
            free.rank_key < paid.rank_key,
            "the free plan first at a full tie"
        );
        // A better supply key outweighs any cost.
        let paid_better = scorer
            .score_group_plan(&plan(&[LINE]), &world(vec![short("3")]), &group())
            .await
            .expect("score");
        assert!(paid_better.rank_key < free.rank_key);
        // The plain joint score keeps its three fields.
        let plain = scorer
            .score_group(&world(vec![short("13")]), &group())
            .await
            .expect("score");
        assert_eq!(
            format!("{}|0000000000000.00", plain.rank_key),
            free.rank_key
        );
    }

    #[tokio::test]
    async fn the_joint_score_is_worded_with_the_points_still_at_risk_first() {
        let scorer = PortfolioScorer::new(rules());
        let score = scorer
            .score_group(
                &Canned {
                    flagged: vec![SO],
                    risk: vec![short("13")],
                    ..Canned::default()
                },
                &group(),
            )
            .await
            .expect("score");
        let worded = scorer.describe(&score).expect("worded");
        assert!(
            worded.starts_with("1 of 2 points still at risk · "),
            "{worded}"
        );
        assert!(worded.contains("13 units short"), "{worded}");
        assert!(!worded.contains('='), "{worded}");
    }

    #[test]
    fn manufacturing_members_route_to_the_process_agent() {
        assert!(is_process_member(&EntityUri(MO.to_owned())));
        assert!(!is_process_member(&EntityUri(SO.to_owned())));
    }

    /// The agent's joint slate over fakes: the real gateway served in-process over an
    /// in-memory world, and a scripted reasoner that hands in plans through it.
    mod agent_slate {
        use std::sync::Mutex;
        use std::time::Duration;

        use ekg_cognition::tools::{
            GatewayConfig, RunLedger, SkillPin, TaskTokens, gateway_router,
        };
        use ekg_cognition::{
            AgentLimits, AlwaysProceed, Reasoner, ReasoningTask, RunReport, SkillPins, Termination,
        };
        use ekg_domain::fakes::{InMemorySimulator, InMemoryWorldModel};
        use ekg_domain::{GroupSlateProposer as _, SourceRecord, TraceTag};
        use rmcp::model::CallToolRequestParams;
        use rmcp::service::RunningService;
        use rmcp::transport::StreamableHttpClientTransport;
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        use rmcp::{RoleClient, ServiceExt as _};
        use serde_json::{Value, json};

        use super::*;

        const MO_7: &str = "https://ekg.example/id/mrp.production/7";
        const CENTRE: &str = "https://ekg.example/id/mrp.workcenter/2";

        /// Plays the runtime's agent over MCP with the task's token: a planner hands in the next
        /// scripted plan.
        struct Scripted {
            url: String,
            plans: Vec<Value>,
            /// The planners' tasks.
            tasks: Mutex<Vec<String>>,
            /// The step limit and run cap of each planner.
            limits: Mutex<Vec<(u32, Duration)>>,
        }

        /// A semantic layer that holds both members at risk.
        struct AtRisk;

        #[async_trait]
        impl ekg_cognition::tools::FindingSource for AtRisk {
            async fn derive(&self, member: &EntityUri) -> Result<ekg_cognition::tools::Derived> {
                Ok(ekg_cognition::tools::Derived::Decided(
                    ekg_cognition::tools::Finding {
                        member: member.clone(),
                        state: ekg_cognition::tools::MemberState::AtRisk,
                        causes: Vec::new(),
                        origin: ekg_cognition::tools::FindingOrigin::Semantic,
                        dropped: Vec::new(),
                    },
                ))
            }

            async fn conflicts(
                &self,
                _member: &EntityUri,
                _claim: &ekg_cognition::tools::Finding,
            ) -> Result<Vec<String>> {
                Ok(Vec::new())
            }
        }

        impl Scripted {
            /// What a planner does.
            async fn plan(&self, client: &RunningService<RoleClient, ()>, task: &ReasoningTask) {
                let index = {
                    let mut tasks = self.tasks.lock().expect("tasks");
                    tasks.push(task.task.clone());
                    self.limits
                        .lock()
                        .expect("limits")
                        .push((task.max_steps, task.run_cap));
                    (tasks.len() - 1).min(self.plans.len() - 1)
                };
                let Value::Object(args) = self.plans[index].clone() else {
                    panic!("a plan is an object");
                };
                let submitted = client
                    .call_tool(CallToolRequestParams::new("submit_plan").with_arguments(args))
                    .await
                    .expect("call");
                assert_ne!(submitted.is_error, Some(true), "accepted");
            }
        }

        #[async_trait]
        impl Reasoner for Scripted {
            async fn reason(&self, task: &ReasoningTask) -> Result<RunReport> {
                let client = ()
                    .serve(StreamableHttpClientTransport::from_config(
                        StreamableHttpClientTransportConfig::with_uri(self.url.clone())
                            .auth_header(task.token.clone()),
                    ))
                    .await
                    .expect("connect");
                self.plan(&client, task).await;
                Ok(RunReport {
                    termination: Termination::Submitted,
                    steps: 2,
                    input_tokens: 100,
                    output_tokens: 20,
                    model: "scripted".to_owned(),
                    closing_answer: None,
                })
            }
        }

        /// Keeps each run's rejected submissions.
        #[derive(Default)]
        struct Rejections(Mutex<Vec<Vec<Vec<String>>>>);

        impl RunRecorder for Rejections {
            fn record(&self, record: ekg_cognition::RunRecord) {
                self.0.lock().expect("runs").push(record.rejections);
            }
        }

        struct NoPins;

        #[async_trait]
        impl SkillPins for NoPins {
            async fn pins(&self, _profile: &str) -> Result<Vec<SkillPin>> {
                Ok(Vec::new())
            }
        }

        fn record(model: &str, id: i64, fields: &[(&str, &str)]) -> SourceRecord {
            SourceRecord {
                source_ref: SourceRef {
                    system: "odoo".to_owned(),
                    model: model.to_owned(),
                    id,
                },
                fields: fields
                    .iter()
                    .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                    .collect(),
            }
        }

        /// A rule flagging the member a projected row blocks, until that row is given a new
        /// date.
        fn blocking_rule() -> Arc<[RiskRule]> {
            Arc::from(vec![RiskRule::new("BLOCKED", |row| {
                if row.contains_key("new_date") {
                    return None;
                }
                Some(DriftVerdict {
                    subject: EntityUri(row.get("blocks")?.clone()),
                    reason: "blocked".to_owned(),
                })
            })])
        }

        #[tokio::test]
        #[allow(clippy::too_many_lines)] // one fixture: world, gateway, seams, one slate
        async fn the_agents_joint_slate_ranks_the_plan_that_clears_every_member_first() {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let memory = Arc::new(InMemoryWorldModel::default());
            for rec in [
                record("sale.order", 1, &[("state", "late"), ("inbound", LINE)]),
                record("purchase.order.line", 4, &[("date_planned", "2027-10-27")]),
                record(
                    "mrp.production",
                    7,
                    &[("work_order", WO), ("work_centre", CENTRE)],
                ),
                record("mrp.workorder", 3, &[("state", "ready")]),
                record("mrp.workcenter", 2, &[("name", "Assembly")]),
            ] {
                memory.materialize(&rec).await.expect("materialize");
            }
            let world: Arc<dyn WorldModel> = memory;
            let profiles = Arc::new(propose::tool_profiles(&world, 4).expect("profiles"));
            let tokens =
                Arc::new(TaskTokens::new(b"joint-slate-suite-secret-32-bytes!").expect("secret"));
            let ledger = Arc::new(RunLedger::default());
            let router = gateway_router(
                Arc::clone(&profiles),
                Arc::clone(&tokens),
                GatewayConfig::default()
                    .with_call_log(Arc::new(|_| {}))
                    .with_run_ledger(Arc::clone(&ledger)),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            let port = listener.local_addr().expect("addr").port();
            tokio::spawn(async move {
                axum::serve(listener, router).await.expect("serve");
            });

            let expedite = json!({
                "action_type": "expedite_po", "target": LINE,
                "params": { "new_date": "2027-10-24 08:00:00" }
            });
            let reschedule = json!({
                "action_type": "reschedule_work_order", "target": WO,
                "params": { "new_date": "2027-10-21 08:00:00" }
            });
            let reasoner = Arc::new(Scripted {
                url: format!("http://127.0.0.1:{port}/mcp"),
                plans: vec![
                    json!({ "actions": [expedite], "explanation": "Pull the line in." }),
                    json!({ "actions": [expedite, reschedule], "explanation": "Both." }),
                ],
                tasks: Mutex::new(Vec::new()),
                limits: Mutex::new(Vec::new()),
            });
            let members = [MO_7, SO];
            let simulator = InMemorySimulator::default()
                .with_entity(
                    EntityUri(LINE.to_owned()),
                    BTreeMap::from([("blocks".to_owned(), SO.to_owned())]),
                )
                .with_entity(
                    EntityUri(WO.to_owned()),
                    BTreeMap::from([("blocks".to_owned(), MO_7.to_owned())]),
                );
            let profile = |id: &str| profiles.get(id).expect("registered");
            let seams = AgentSeams {
                world: Arc::clone(&world),
                simulator: Arc::new(simulator),
                supply_risk: profile(propose::SUPPLY_RISK_PROFILE),
                process: profile(propose::PROCESS_PROFILE),
                portfolio: profile(propose::PORTFOLIO_PROFILE),
                rules: blocking_rule(),
                tokens,
                reasoner: Arc::clone(&reasoner) as Arc<dyn Reasoner>,
                ledger,
                pins: Arc::new(NoPins),
                limits: AgentLimits {
                    max_steps: 4,
                    run_cap: Duration::from_secs(60),
                },
                joint_limits: AgentLimits {
                    max_steps: 9,
                    run_cap: Duration::from_secs(150),
                },
                analyst_limits: AgentLimits {
                    max_steps: 7,
                    run_cap: Duration::from_secs(90),
                },
                supply_risk_analyst: profile(propose::SUPPLY_RISK_ANALYST_PROFILE),
                process_analyst: profile(propose::PROCESS_ANALYST_PROFILE),
                findings: Arc::new(AtRisk),
                planner_strategies: Vec::new(),
                planner_reads: false,
                text_max_chars: 80,
                budget: Duration::from_secs(30),
            };
            let group = SubjectGroup {
                key: "portfolio/0123456789ab".to_owned(),
                members: members.iter().map(|m| EntityUri((*m).to_owned())).collect(),
                links: vec![format!("linked: {MO_7} fulfils {SO}")],
                worded: false,
            };
            let gate: Arc<dyn RunGate> = Arc::new(AlwaysProceed);
            let rejections = Arc::new(Rejections::default());
            let recorder = Arc::clone(&rejections) as Arc<dyn RunRecorder>;
            let slate = agent_portfolio_slate_agent(&seams, &gate, &recorder)
                .propose_group_slate(&group)
                .await
                .expect("a joint slate");

            assert_eq!(slate.trace.steps[0].tag, TraceTag::Graph);
            assert!(slate.trace.steps[0].text.starts_with("Linked: "));
            let verbs = |index: usize| -> Vec<&str> {
                slate.options[index]
                    .plan
                    .actions
                    .iter()
                    .map(|a| a.action_type.0.as_str())
                    .collect()
            };
            assert_eq!(verbs(0), ["expedite_po", "reschedule_work_order"]);
            assert_eq!(verbs(1), ["expedite_po"]);
            assert!(slate.options[0].recommended);
            let at_risk =
                |index: usize| slate.options[index].score.metrics["members_at_risk"].as_str();
            assert_eq!((at_risk(0), at_risk(1)), ("0", "1"));
            // The second run was told what the first plan achieved.
            let tasks = reasoner.tasks.lock().expect("tasks");
            assert!(
                tasks[0].contains(MO_7) && tasks[0].contains(SO),
                "{}",
                tasks[0]
            );
            assert!(tasks[1].contains("members_at_risk=1"), "{}", tasks[1]);
            // Each run was under a portfolio's limits, not a point's.
            let limits = reasoner.limits.lock().expect("limits");
            assert!(
                limits.len() >= 2 && limits.iter().all(|l| *l == (9, Duration::from_secs(150))),
                "{limits:?}"
            );
        }
    }
}
