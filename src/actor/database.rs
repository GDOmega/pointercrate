use actix::{Actor, Addr, Handler, Message, SyncArbiter, SyncContext};
use crate::{
    config::{EXTENDED_LIST_SIZE, LIST_SIZE},
    error::PointercrateError,
    middleware::auth::{Authorization, Claims},
    model::{
        record::{RecordStatus, Submission},
        user::{PatchMe, PermissionsSet, Registration},
        Demon, Player, Record, Submitter, User,
    },
    pagination::Paginatable,
    patch::{Patch as PatchTrait, PatchField, Patchable},
    video, Result,
};
use diesel::{
    pg::PgConnection,
    r2d2::{ConnectionManager, Pool},
    result::Error,
    RunQueryDsl,
};
use ipnetwork::IpNetwork;
use log::{debug, info};

/// Actor that executes database related actions on a thread pool
#[allow(missing_debug_implementations)]
pub struct DatabaseActor(pub Pool<ConnectionManager<PgConnection>>);

impl DatabaseActor {
    pub fn from_env() -> Addr<Self> {
        info!("Initializing pointercrate database connection pool");

        let database_url = std::env::var("DATABASE_URL").expect("DATABASE_URL is not set");
        let manager = ConnectionManager::<PgConnection>::new(database_url);
        let pool = Pool::builder()
            .build(manager)
            .expect("Failed to create database connection pool");

        SyncArbiter::start(4, move || DatabaseActor(pool.clone()))
    }
}

impl Actor for DatabaseActor {
    type Context = SyncContext<Self>;

    fn started(&mut self, _ctx: &mut Self::Context) {
        info!("Started pointercrate database actor! We can now interact with the database!")
    }

    fn stopped(&mut self, _ctx: &mut Self::Context) {
        info!(
            "Stopped pointercrate database actor! We can no longer interact with the database! :("
        )
    }
}

/// Message that indicates the [`DatabaseActor`] to retrieve a [`Submitter`] object based on the
/// given [IP-Address](`IpNetwork`).
///
/// If no submitter with the given IP is known, a new object will be crated an inserted into the
/// database
#[derive(Debug)]
pub struct SubmitterByIp(pub IpNetwork);

/// Message that indicates the [`DatabaseActor`] to retrieve a [`Player`] object with the given name
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no player with the given name exist.
#[derive(Debug)]
pub struct PlayerByName(pub String);

/// Message that indicates the [`DatabaseActor`] to retrieve a [`Demon`] object with the given name
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no demon with the given name exist.
#[derive(Debug)]
pub struct DemonByName(pub String);

/// Message that indicates the [`DatabaseActor`] to retrieve a `(Player, Demon)` pair whose names
/// match the given string.
///
/// This is basically the same as sending a [`PlayerByName`] message followed by a [`DemonByName`]
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should either not player or no demon with the given name
/// exist.
#[derive(Debug)]
pub struct ResolveSubmissionData(pub String, pub String);

/// Message that indicates the [`DatabaseActor`] that a record has been submitted by the given
/// [`Submitter`] and should be processed
///
/// ## Errors
/// + [`PointercrateError::BannedFromSubmissions`]: The given submitter has been banned from
/// submitting records
/// + [`PointercrateError::PlayerBanned`]: The player the record was submitted
/// for has been banned from having records on the list
/// + [`PointercrateError::SubmitLegacy`]: The demon the record was submitted for is on the legacy
/// list
/// + [`PointercrateError::Non100Extended`]: The demon the record was submitted for is on the
/// extended list, and `progress` isn't 100
/// + [`PointercrateError::InvalidProgress `]: The submission progress is lower than the
/// demons `record_requirement`
/// + [`PointercrateError::SubmissionExists`]: If a matching record is
/// already in the database, and it's either [rejected](`RecordStatus::Rejected`), or has higher
/// progress than the submission.
/// + Any error returned by [`video::validate`]
#[derive(Debug)]
pub struct ProcessSubmission(pub Submission, pub Submitter);

/// Message that indicates the [`DatabaseActor`] to retrieve a [`Record`] object with the given id.
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no record with the given id exist.
#[derive(Debug)]
pub struct RecordById(pub i32);

/// Message that indicates the [`DatabaseActor`] to delete the [`Record`] object with the given id.
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no record with the given id exist.
#[derive(Debug)]
pub struct DeleteRecordById(pub i32);

/// Message that indicates the [`DatabaseActor`] to process the given [`Registration`]
///
/// ## Errors
/// + [`PointercrateError::InvalidUsername`]: If the username is shorter than 3 characters or
/// starts/end with spaces
/// + [`PointercrateError::InvalidPassword`]: If the password is shorter than
/// 10 characters
/// + [`PointercrateError::NameTaken`]: If the username is already in use by another
/// account
#[derive(Debug)]
pub struct Register(pub Registration);

/// Message that indicates the [`DatabaseActor`] to retrieve a [`User`] object with the given id.
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no user with the given id exist.
#[derive(Debug)]
pub struct UserById(pub i32);

/// Message that indicates the [`DatabaseActor`] to retrieve a [`User`] object with the given name.
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no user with the given name exist.
#[derive(Debug)]
pub struct UserByName(pub String);

/// Message that indicates the [`DatabaseActor`] to delete the [`User`] object with the given id.
///
/// ## Errors
/// + [`PointercrateError::ModelNotFound`]: Should no user with the given id exist.
#[derive(Debug)]
pub struct DeleteUserById(pub i32);

/// Message that indicates the [`DatabaseActor`] to authorize a [`User`] by access token
///
/// ## Errors
/// + [`PointercrateError::Unauthorized`]: Authorization failed
#[derive(Debug)]
pub struct TokenAuth(pub Authorization);

/// Message that indicates the [`DatabaseActor`] to authorize a [`User`] using basic auth
///
/// ## Errors
/// + [`PointercrateError::Unauthorized`]: Authorization failed
#[derive(Debug)]
pub struct BasicAuth(pub Authorization);

/// Message that indicates the [`DatabaseActor`] to invalidate all access tokens to the account
/// authorized by the given [`Authorization`] object. The [`Authorization`] object must be of type
/// [`Authorization::Basic] for this.
///
/// Invalidation is done by re-randomizing the salt used for hashing the user's password (since the
/// key tokens are signed with contains the salt, this will invalidate all old access tokens).
///
/// ## Errors
/// + [`PointercrateError::Unauthorized`]: Authorization failed
#[derive(Debug)]
pub struct Invalidate(pub Authorization);

/// Message that indicates the [`DatabaseActor`] to perform an patch
///
/// A Patch is done in 3 steps:
/// + First, we check if the given [`User`] has the required permissions to perform the patch
/// (Authorization)
/// + Second, we perform the patch in-memory on the given target, validating it
/// + Last, we write the successfull patch into the database
#[allow(missing_debug_implementations)]
pub struct Patch<Target, Patch>(pub User, pub Target, pub Patch)
where
    Target: Patchable<Patch>,
    Patch: PatchTrait;

/// Specialized patch message used when patch target is the user performing the patch
///
/// This is needed because `User` and `Target` in [`Patch`] would have to be the same object,
/// something the rust ownership (rightfully so) doesn't allow. To prevent a needless clone of the
/// user object, we introduce this specialized message
#[derive(Debug)]
pub struct PatchCurrentUser(pub User, pub PatchMe);

#[derive(Debug)]
pub struct Paginate<P: Paginatable>(pub P);

impl Message for SubmitterByIp {
    type Result = Result<Submitter>;
}

impl Handler<SubmitterByIp> for DatabaseActor {
    type Result = Result<Submitter>;

    fn handle(&mut self, msg: SubmitterByIp, _ctx: &mut Self::Context) -> Self::Result {
        debug!(
            "Attempt to retrieve submitter with IP '{}', creating if not exists!",
            msg.0
        );

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match Submitter::by_ip(&msg.0).first(connection) {
            Ok(submitter) => Ok(submitter),
            Err(Error::NotFound) =>
                Submitter::insert(connection, &msg.0).map_err(PointercrateError::database),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for PlayerByName {
    type Result = Result<Player>;
}

impl Handler<PlayerByName> for DatabaseActor {
    type Result = Result<Player>;

    fn handle(&mut self, msg: PlayerByName, _ctx: &mut Self::Context) -> Self::Result {
        debug!(
            "Attempt to retrieve player with name '{}', creating if not exists!",
            msg.0
        );

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match Player::by_name(&msg.0).first(connection) {
            Ok(player) => Ok(player),
            Err(Error::NotFound) =>
                Player::insert(connection, &msg.0).map_err(PointercrateError::database),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for DemonByName {
    type Result = Result<Demon>;
}

impl Handler<DemonByName> for DatabaseActor {
    type Result = Result<Demon>;

    fn handle(&mut self, msg: DemonByName, _ctx: &mut Self::Context) -> Self::Result {
        debug!("Attempting to retrieve demon with name '{}'!", msg.0);

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match Demon::by_name(&msg.0).first(connection) {
            Ok(demon) => Ok(demon),
            Err(Error::NotFound) =>
                Err(PointercrateError::ModelNotFound {
                    model: "Demon",
                    identified_by: msg.0,
                }),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for ResolveSubmissionData {
    type Result = Result<(Player, Demon)>;
}

impl Handler<ResolveSubmissionData> for DatabaseActor {
    type Result = Result<(Player, Demon)>;

    fn handle(&mut self, msg: ResolveSubmissionData, ctx: &mut Self::Context) -> Self::Result {
        debug!(
            "Attempt to resolve player '{}' and demon '{}' for a submission!",
            msg.0, msg.1
        );

        let (player, demon) = (msg.0, msg.1);

        let player = self.handle(PlayerByName(player), ctx)?;
        let demon = self.handle(DemonByName(demon), ctx)?;

        Ok((player, demon))
    }
}

impl Message for ProcessSubmission {
    type Result = Result<Option<Record>>;
}

impl Handler<ProcessSubmission> for DatabaseActor {
    type Result = Result<Option<Record>>;

    fn handle(&mut self, msg: ProcessSubmission, ctx: &mut Self::Context) -> Self::Result {
        debug!("Processing submission {:?}", msg.0);

        if msg.1.banned {
            return Err(PointercrateError::BannedFromSubmissions)?
        }

        let Submission {
            progress,
            player,
            demon,
            video,
            verify_only,
        } = msg.0;

        let video = match video {
            Some(ref video) => Some(video::validate(video)?),
            None => None,
        };

        let (player, demon) = self.handle(ResolveSubmissionData(player, demon), ctx)?;

        if player.banned {
            return Err(PointercrateError::PlayerBanned)
        }

        if demon.position > *EXTENDED_LIST_SIZE {
            return Err(PointercrateError::SubmitLegacy)
        }

        if demon.position > *LIST_SIZE && progress != 100 {
            return Err(PointercrateError::Non100Extended)
        }

        if progress > 100 || progress < demon.requirement {
            return Err(PointercrateError::InvalidProgress {
                requirement: demon.requirement,
            })?
        }

        debug!("Submission is valid, checking for duplicates!");

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        let record: std::result::Result<Record, _> = match video {
            Some(ref video) =>
                Record::get_existing(player.id, &demon.name, video).first(connection),
            None => Record::by_player_and_demon(player.id, &demon.name).first(connection),
        };

        let video_ref = video.as_ref().map(AsRef::as_ref);

        let id = match record {
            Ok(record) =>
                if record.status() != RecordStatus::Rejected && record.progress() < progress {
                    if verify_only {
                        return Ok(None)
                    }

                    if record.status() == RecordStatus::Submitted {
                        debug!(
                            "The submission is duplicated, but new one has higher progress. Deleting old with id {}!",
                            record.id
                        );

                        record
                            .delete(connection)
                            .map_err(PointercrateError::database)?;
                    }

                    debug!(
                        "Duplicate {} either already accepted, or has lower progress, accepting!",
                        record.id
                    );

                    Record::insert(
                        connection,
                        progress,
                        video_ref,
                        player.id,
                        msg.1.id,
                        &demon.name,
                    )
                    .map_err(PointercrateError::database)?
                } else {
                    return Err(PointercrateError::SubmissionExists {
                        status: record.status(),
                        existing: record.id,
                    })
                },
            Err(Error::NotFound) => {
                debug!("No duplicate found, accepting!");

                if verify_only {
                    return Ok(None)
                }

                Record::insert(
                    connection,
                    progress,
                    video_ref,
                    player.id,
                    msg.1.id,
                    &demon.name,
                )
                .map_err(PointercrateError::database)?
            },
            Err(err) => return Err(PointercrateError::database(err)),
        };

        info!("Submission successful! Created new record with ID {}", id);

        Ok(Some(Record {
            id,
            progress,
            video,
            status: RecordStatus::Submitted,
            player,
            submitter: msg.1.id,
            demon: demon.into(),
        }))
    }
}

impl Message for RecordById {
    type Result = Result<Record>;
}

impl Handler<RecordById> for DatabaseActor {
    type Result = Result<Record>;

    fn handle(&mut self, msg: RecordById, _: &mut Self::Context) -> Self::Result {
        debug!("Attempt to resolve record by id {}", msg.0);

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match Record::by_id(msg.0).first(connection) {
            Ok(record) => Ok(record),
            Err(Error::NotFound) =>
                Err(PointercrateError::ModelNotFound {
                    model: "Record",
                    identified_by: msg.0.to_string(),
                }),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for DeleteRecordById {
    type Result = Result<()>;
}

impl Handler<DeleteRecordById> for DatabaseActor {
    type Result = Result<()>;

    fn handle(&mut self, msg: DeleteRecordById, _: &mut Self::Context) -> Self::Result {
        info!("Deleting record with ID {}!", msg.0);

        self.0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)
            .and_then(|connection| {
                Record::delete_by_id(&connection, msg.0).map_err(PointercrateError::database)
            })
    }
}

impl Message for UserById {
    type Result = Result<User>;
}

impl Handler<UserById> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: UserById, _: &mut Self::Context) -> Self::Result {
        debug!("Attempt to resolve user by id {}", msg.0);

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match User::by_id(msg.0).first(connection) {
            Ok(user) => Ok(user),
            Err(Error::NotFound) =>
                Err(PointercrateError::ModelNotFound {
                    model: "User",
                    identified_by: msg.0.to_string(),
                }),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for UserByName {
    type Result = Result<User>;
}

impl Handler<UserByName> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: UserByName, _: &mut Self::Context) -> Self::Result {
        debug!("Attempt to resolve user by name {}", msg.0);

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match User::by_name(&msg.0).first(connection) {
            Ok(user) => Ok(user),
            Err(Error::NotFound) =>
                Err(PointercrateError::ModelNotFound {
                    model: "User",
                    identified_by: msg.0,
                }),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for TokenAuth {
    type Result = Result<User>;
}

// During authorization, all and every error that might come up will be converted into
// `PointercrateError::Unauthorized`
impl Handler<TokenAuth> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: TokenAuth, ctx: &mut Self::Context) -> Self::Result {
        debug!("Attempting to perform token authorization (we're not logging the token for obvious reasons smh)");

        if let Authorization::Token(token) = msg.0 {
            // Well this is reassuring. Also we directly deconstruct it and only save the ID so we
            // don't accidentally use unsafe values later on
            let Claims { id, .. } = jsonwebtoken::dangerous_unsafe_decode::<Claims>(&token)
                .map_err(|_| PointercrateError::Unauthorized)?
                .claims;

            debug!("The token identified the user with id {}", id);

            let user = self
                .handle(UserById(id), ctx)
                .map_err(|_| PointercrateError::Unauthorized)?;

            user.validate_token(&token)
        } else {
            Err(PointercrateError::Unauthorized)
        }
    }
}

impl Message for BasicAuth {
    type Result = Result<User>;
}

impl Handler<BasicAuth> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: BasicAuth, ctx: &mut Self::Context) -> Self::Result {
        debug!("Attempting to perform basic authorization (we're not logging the password for even more obvious reasons smh)");

        if let Authorization::Basic(username, password) = msg.0 {
            debug!(
                "Trying to authorize user {} (still not logging the password)",
                username
            );

            let user = self
                .handle(UserByName(username), ctx)
                .map_err(|_| PointercrateError::Unauthorized)?;

            user.verify_password(&password)
        } else {
            Err(PointercrateError::Unauthorized)
        }
    }
}

impl Message for Register {
    type Result = Result<User>;
}

impl Handler<Register> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, msg: Register, _: &mut Self::Context) -> Self::Result {
        if msg.0.name.len() < 3 || msg.0.name != msg.0.name.trim() {
            return Err(PointercrateError::InvalidUsername)
        }

        if msg.0.password.len() < 10 {
            return Err(PointercrateError::InvalidPassword)
        }

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        match User::by_name(&msg.0.name).first::<User>(connection) {
            Ok(_) => Err(PointercrateError::NameTaken),
            Err(Error::NotFound) =>
                User::register(connection, &msg.0).map_err(PointercrateError::database),
            Err(err) => Err(PointercrateError::database(err)),
        }
    }
}

impl Message for DeleteUserById {
    type Result = Result<()>;
}

impl Handler<DeleteUserById> for DatabaseActor {
    type Result = Result<()>;

    fn handle(&mut self, msg: DeleteUserById, _: &mut Self::Context) -> Self::Result {
        info!("Deleting user with ID {}!", msg.0);

        self.0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)
            .and_then(|connection| {
                User::delete_by_id(&connection, msg.0).map_err(PointercrateError::database)
            })
    }
}

impl<T, P> Message for Patch<T, P>
where
    T: Patchable<P> + 'static,
    P: PatchTrait,
{
    type Result = Result<T>;
}

impl<T, P> Handler<Patch<T, P>> for DatabaseActor
where
    T: Patchable<P> + 'static,
    P: PatchTrait,
{
    type Result = Result<T>;

    fn handle(&mut self, mut msg: Patch<T, P>, _: &mut Self::Context) -> Self::Result {
        // TODO: use transactions here and return 409 CONFLICT in case of transaction failure
        let required = msg.2.required_permissions();

        if msg.0.permissions() & required != required {
            return Err(PointercrateError::MissingPermissions {
                required: PermissionsSet::one(required),
            })
        }

        // Modify the object we're currently working with to validate the values
        msg.1.apply_patch(msg.2)?;

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        // Store the modified object in the database
        msg.1.update_database(connection)?;

        Ok(msg.1)
    }
}

impl Message for PatchCurrentUser {
    type Result = Result<User>;
}

impl Handler<PatchCurrentUser> for DatabaseActor {
    type Result = Result<User>;

    fn handle(&mut self, mut msg: PatchCurrentUser, _: &mut Self::Context) -> Self::Result {
        // TODO: transaction
        msg.0.apply_patch(msg.1)?;

        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        msg.0.update_database(connection)?;

        Ok(msg.0)
    }
}

impl Message for Invalidate {
    type Result = Result<()>;
}

impl Handler<Invalidate> for DatabaseActor {
    type Result = Result<()>;

    fn handle(&mut self, msg: Invalidate, ctx: &mut Self::Context) -> Self::Result {
        let password = if let Authorization::Basic(_, ref password) = msg.0 {
            password.clone()
        } else {
            return Err(PointercrateError::Unauthorized)
        };

        let user = self.handle(BasicAuth(msg.0), ctx)?;

        let patch = PatchMe {
            password: PatchField::Some(password),
            display_name: PatchField::Absent,
            youtube_channel: PatchField::Absent,
        };

        self.handle(PatchCurrentUser(user, patch), ctx).map(|_| ())
    }
}

impl<P: Paginatable + 'static> Message for Paginate<P> {
    type Result = Result<(Vec<P::Result>, String)>;
}

impl<P: Paginatable + 'static> Handler<Paginate<P>> for DatabaseActor {
    type Result = Result<(Vec<P::Result>, String)>;

    fn handle(&mut self, msg: Paginate<P>, _: &mut Self::Context) -> Self::Result {
        let connection = &*self
            .0
            .get()
            .map_err(|_| PointercrateError::DatabaseConnectionError)?;

        let first = msg.0.first(connection)?;
        let last = msg.0.last(connection)?;
        let next = msg.0.next_after(connection)?;
        let prev = msg.0.prev_before(connection)?;

        let result = msg.0.result(connection)?;

        // TODO: compare last thing in our list with last and first thing in our list with first
        // and then only generate the needed headers

        let header = format! {
            "<{}>; rel=first,<{}>; rel=prev,<{}>; rel=next,<{}>; rel=last",
            serde_urlencoded::ser::to_string(first).unwrap(),
            serde_urlencoded::ser::to_string(prev).unwrap(),
            serde_urlencoded::ser::to_string(next).unwrap(),
            serde_urlencoded::ser::to_string(last).unwrap(),
        };

        Ok((result, header))
    }
}
